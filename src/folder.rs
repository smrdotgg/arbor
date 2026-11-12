//! A folder on screen: the live listing of a pane's visible tab, with its cursor, selection,
//! filter, sort and history. And [`TabMemo`], all a hidden tab keeps.
//!
//! NORTHSTAR bans saved listings, so a hidden tab keeps no entries: only its path, history,
//! selected names, scroll offset, filter and sort. When it's shown again, its folder is read
//! fresh (`State::show_tab`), and the selection and scroll come back once the read lands.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use rune::{Latest, Tx};

use crate::fs::{self, Entry, Listing, Sort, SortKey};
pub use crate::marked::Picked;
use crate::marked::{Bits, Marked, Rows};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Nav {
    New,
    Back,
    Forward,
    Reload,
    /// A hidden tab becoming visible: no history change, its filter kept, its scroll restored.
    Show,
    /// Opened for another app (`incoming.rs`): like `New`, but the tab's history starts here.
    Fresh,
}

pub(crate) struct Pending {
    /// Which request this is ([`next_request`]): only its own answer lands.
    pub req: u64,
    pub path: PathBuf,
    pub nav: Nav,
    /// Names to select once the listing lands (the selection, exactly: those still there);
    /// the cursor goes on `cursor`, else the first. Only the cursor's name: just it selected,
    /// as after a click.
    pub select: Vec<OsString>,
    /// Where the cursor goes, when not on the first of `select` (it may be deselected).
    pub cursor: Option<OsString>,
    /// Nothing was selected (a reload or a hidden tab of a folder with nothing selected): it
    /// lands with nothing selected, even where an item appeared (a folder newly opened selects
    /// its first row).
    pub none: bool,
    /// Start renaming the first selected name once it lands (a new folder).
    pub rename: bool,
    /// Where to scroll the list once it lands ([`Nav::Show`]).
    pub scroll: f32,
    /// [`Folder::picks`] when it was asked for: a reload that lands after a newer pick keeps
    /// the user's choice instead of `select`.
    pub picks: u64,
    /// The identities of `select` and the cursor's item, when known (a hidden tab's): the
    /// read finds an item renamed since under its new name.
    pub ids: Ids,
}

/// The identities of the items a read is to select, beside their names ([`Pending::select`],
/// [`Pending::cursor`]); empty when unknown (what an operation made: found by name).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ids {
    pub select: Vec<Option<fs::Ident>>,
    pub cursor: Option<fs::Ident>,
}

impl Ids {
    pub fn is_empty(&self) -> bool {
        self.cursor.is_none() && self.select.iter().all(Option::is_none)
    }
}

/// An item to select: its name, and its identity when known.
pub(crate) type Named = (OsString, Option<fs::Ident>);

impl Pending {
    /// Where the cursor goes once it lands.
    pub fn cursor_name(&self) -> Option<&OsString> {
        self.cursor.as_ref().or(self.select.first())
    }

    /// Whether it lands with items marked (two or more, or one the cursor isn't on), rather
    /// than the cursor alone selected.
    pub fn marks(&self) -> bool {
        Pending::marks_for(&self.select, self.cursor.as_ref())
    }

    pub(crate) fn marks_for(select: &[OsString], cursor: Option<&OsString>) -> bool {
        select.len() > 1 || (select.len() == 1 && cursor.is_some_and(|c| *c != select[0]))
    }

    /// What its read is to select, worked out on the worker: by name and identity when the
    /// identities are known, else by name.
    fn pick(&self) -> Pick {
        if !self.ids.is_empty() {
            let mut ids = self.ids.select.iter().copied().chain(std::iter::repeat(None));
            let select = self.select.iter().map(|n| (n.clone(), ids.next().flatten())).collect();
            return Pick::Chosen(select, self.cursor.clone().map(|c| (c, self.ids.cursor)));
        }
        if self.marks() { Pick::Names(self.select.clone()) } else { Pick::Nothing }
    }
}

/// What a hidden tab keeps. No entries: its folder is read again when it's shown.
#[derive(Clone, Debug, PartialEq)]
pub struct TabMemo {
    pub path: PathBuf,
    pub back: Vec<PathBuf>,
    pub fwd: Vec<PathBuf>,
    /// The selection, exactly, in display order (only the cursor's name: just it).
    pub select: Vec<OsString>,
    /// The cursor's item (which may be deselected).
    pub cursor: Option<OsString>,
    /// Nothing was selected.
    pub none: bool,
    /// How far the list was scrolled, in points.
    pub scroll: f32,
    pub filter: String,
    /// The go-to field was open (`Folder::goto`), with `filter` its text (or emptied).
    pub goto: bool,
    pub sort: Sort,
    /// The user chose this tab's sort (see [`Folder::sort_set`]).
    pub sort_set: bool,
    /// It was showing a search for this: showing the tab again searches again (nothing of
    /// the results is kept).
    pub search: Option<String>,
    /// List, icons, columns or tiles.
    pub view: crate::grid::ViewMode,
    /// The identities of `select` and the cursor's item: shown again, the tab finds an item
    /// renamed meanwhile under its new name ([`Pending::ids`]).
    pub ids: Ids,
}

/// A finished read, tagged with the request that asked for it and the sort it was sorted by.
pub struct Answer {
    pub req: u64,
    pub sort: Sort,
    pub listing: Listing,
    /// Its entries that are to be selected (`Pending::marks`), found on the worker, so landing
    /// a reload of a big folder with a big selection doesn't look through it on the window's
    /// thread.
    pub marks: Option<Bits>,
    /// The selection it was asked to keep ([`Kept`], [`Pick::Chosen`]), found on the worker
    /// by name and identity: the names, and the cursor's (`Pending::select`, `Pending::cursor`).
    pub chosen: Option<(Vec<OsString>, Option<OsString>)>,
}

/// What a read is to select once it lands, worked out on its worker.
pub(crate) enum Pick {
    Nothing,
    /// These names (to mark: two or more, or one the cursor isn't on).
    Names(Vec<OsString>),
    /// The selection on screen when it was asked for, as it was.
    Kept(Kept),
    /// These items (and the cursor's), by name and identity (a hidden tab shown again).
    Chosen(Vec<Named>, Option<Named>),
}

/// A row's new name while it can't take it yet ([`Folder::labels`]): only shown; every action
/// on the row waits until the folder is read again.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Label {
    /// The name shown on its row.
    pub name: String,
    /// Its index in the listing when the rename started (checked before it's used).
    pub at: Option<usize>,
    /// An inline rename whose outcome hasn't come back (else, one that landed while the listing
    /// was shared: the read that follows brings it).
    pub waiting: bool,
    /// The identity the row was read with: a read that lands keeps a label only on that item.
    pub id: Option<fs::Ident>,
}

/// The selection on screen, handed to a worker as it is, without copying a name: the listing
/// and its rows (shared), and the selected rows (a word per 64). The worker reads the names.
pub(crate) struct Kept {
    entries: Arc<Vec<Entry>>,
    rows: Arc<Rows>,
    marked: Marked,
    sel: usize,
    none: bool,
}

impl Kept {
    /// As [`Folder::selection_names`] says it, each with its identity.
    fn named(&self) -> (Vec<Named>, Option<Named>) {
        let name = |r: usize| self.rows.get(r).map(|&i| &self.entries[i as usize]).map(|e| (e.os.clone(), e.id));
        let cursor = name(self.sel);
        let items = if self.marked.is_empty() {
            cursor.clone().into_iter().filter(|_| !self.none).collect()
        } else {
            self.marked.iter().filter_map(name).collect()
        };
        (items, cursor)
    }
}

impl Answer {
    pub fn new(req: u64, sort: Sort, listing: Listing) -> Answer {
        Answer { req, sort, listing, marks: None, chosen: None }
    }

    /// The listing's read of `path`, selecting what `pick` says once it lands (on the worker).
    fn read(req: u64, sort: Sort, path: PathBuf, pick: Pick, stop: &AtomicBool) -> Option<Answer> {
        let mut a = Answer::new(req, sort, fs::list_until(path, sort, stop)?);
        a.pick(pick);
        Some(a)
    }

    /// Works out `pick` on this listing: its names, and the entries to mark. On a worker.
    pub(crate) fn pick(&mut self, pick: Pick) {
        let Ok(entries) = &self.listing.result else { return };
        match pick {
            Pick::Nothing => {}
            Pick::Names(v) => self.marks = Some(mark_named(entries, &v)),
            Pick::Kept(k) => {
                let (select, cursor) = k.named();
                self.choose(select, cursor);
            }
            Pick::Chosen(select, cursor) => self.choose(select, cursor),
        }
    }

    /// Finds `select` and the cursor's item in this listing ([`find_named`]), and the entries
    /// to mark. On a worker.
    fn choose(&mut self, select: Vec<Named>, cursor: Option<Named>) {
        let Ok(entries) = &self.listing.result else { return };
        let (select, cursor) = find_named(entries, select, cursor);
        if Pending::marks_for(&select, cursor.as_ref()) {
            self.marks = Some(mark_named(entries, &select));
        }
        self.chosen = Some((select, cursor));
    }
}

/// Where `select` (and the cursor's item) are in `entries`, by identity: under its name while
/// that's the item (or an identity isn't known); else wherever the item is now (renamed since);
/// else, under its name, another item of the same kind (an editor saves a new file in the old
/// one's place); else it's gone (another kind of item has its name: not selected). The cursor's
/// item, gone, leaves the cursor on its name. Looks through the listing a second time only for
/// items not under their names: on a worker.
pub(crate) fn find_named(entries: &[Entry], select: Vec<Named>, cursor: Option<Named>) -> (Vec<OsString>, Option<OsString>) {
    let same = |a: Option<fs::Ident>, b: Option<fs::Ident>| a.is_none() || b.is_none() || a == b;
    let want: HashMap<&OsStr, Option<fs::Ident>> = select.iter().chain(cursor.iter()).map(|(n, id)| (n.as_os_str(), *id)).collect();
    let (mut here, mut alike): (HashSet<&OsStr>, HashSet<&OsStr>) = Default::default();
    for e in entries {
        if let Some(&id) = want.get(e.os.as_os_str()) {
            if same(id, e.id) {
                here.insert(e.os.as_os_str());
            } else if let (Some(a), Some(b)) = (id, e.id)
                && a.same_kind(&b)
            {
                alike.insert(e.os.as_os_str());
            }
        }
    }
    let lost: HashMap<fs::Ident, OsString> =
        want.iter().filter(|(n, _)| !here.contains(*n)).filter_map(|(n, id)| Some(((*id)?, n.to_os_string()))).collect();
    let mut moved: HashMap<OsString, OsString> = HashMap::new();
    if !lost.is_empty() {
        for e in entries {
            if let Some(id) = e.id
                && let Some(n) = lost.get(&id)
            {
                moved.insert(n.clone(), e.os.clone());
            }
        }
    }
    let unlike: HashSet<OsString> = want
        .iter()
        .filter(|(n, id)| id.is_some() && !here.contains(*n) && !alike.contains(*n) && !moved.contains_key(**n))
        .map(|(n, _)| n.to_os_string())
        .collect();
    let to = |n: OsString| moved.get(&n).cloned().unwrap_or(n);
    let select = select.into_iter().filter(|(n, _)| !unlike.contains(n)).map(|(n, _)| to(n)).collect();
    (select, cursor.map(|(n, _)| to(n)))
}

/// Sorts `entries` by `sort`, `bits` (indices into them: a selection) moving with them.
pub(crate) fn sort_carrying(entries: &mut Vec<Entry>, bits: &mut Bits, sort: Sort) {
    let perm = fs::sort_perm(entries, sort);
    let mut slots: Vec<Option<Entry>> = std::mem::take(entries).into_iter().map(Some).collect();
    *entries = perm.iter().map(|&i| slots[i as usize].take().expect("a permutation")).collect();
    *bits = bits.permuted(&perm);
}

/// The entries of `entries` named in `select` (what's gone isn't). Looks through the whole
/// listing: done on the listing's worker.
pub(crate) fn mark_named(entries: &[Entry], select: &[OsString]) -> Bits {
    #[cfg(test)]
    marked_on::note();
    let want: HashSet<&OsString> = select.iter().collect();
    let mut b = Bits::default();
    for (i, e) in entries.iter().enumerate() {
        if want.contains(&e.os) {
            b.set(i);
        }
    }
    b
}

/// Entry `e` is called `new` now: its name, sort key and (a dot) whether it's hidden; other
/// ways of hiding (a flag) come with the fresh read.
fn set_name(e: &mut Entry, new: &OsStr) {
    let was_dot = e.name.starts_with('.');
    e.os = new.to_os_string();
    e.name = new.to_string_lossy().into_owned();
    e.lower = e.name.to_lowercase();
    e.key = format!("f:{}", e.name);
    e.hidden = e.name.starts_with('.') || (e.hidden && !was_dot);
}

/// The most entries a rename landing looks through on the window's thread (a pass over 50,000
/// takes about 0.35 ms); a bigger listing waits for the read that follows instead.
const SCAN_MAX: usize = 50_000;

/// Every folder's `version` comes from one counter, so a row handler from one folder's frame
/// never matches another folder's version (panes come and go).
static VERSION: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_version() -> u64 {
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
    pub dir_id: Option<crate::noclobber::DirId>,
    /// The whole folder, sorted.
    /// Shared (read-only) with a worker finding the selection in a reload ([`Kept`]): changed
    /// through `Arc::make_mut`, which copies it only while a worker still holds it.
    pub entries: Arc<Vec<Entry>>,
    /// Indices into `entries` that pass the hidden-file and filter rules, in display order,
    /// with running sums of their sizes (`marked.rs`).
    pub visible: Arc<Rows>,
    /// The cursor: index into `visible`.
    pub sel: usize,
    /// Extra selection (⇧-arrows, ⌘-click, ⌘A): rows of `visible`, with their totals
    /// (`marked.rs`). Empty means "just the cursor", unless `none`.
    pub marked: Marked,
    /// Nothing is selected, not even the cursor's row: what was selected went (hidden,
    /// filtered out, gone from the folder) and no other row is chosen in its place. A click,
    /// an arrow or any other choice selects again.
    pub none: bool,
    /// The selection carried by entry while the rows are made again (`marked.rs`): empty
    /// otherwise.
    pub(crate) carry: Bits,
    /// New names shown on rows (by their names in the listing) that can't take them yet: an
    /// inline rename on its way, or a rename that landed while a reload's worker or the other
    /// pane held the listing (changing it then would copy it). Only shown: every action on such
    /// a row is refused ([`Folder::updating`]) until the folder is read again.
    pub(crate) labels: HashMap<OsString, Label>,
    /// A read of the folder on screen (after a change to it) was replaced by a navigation
    /// elsewhere: asked again if that navigation fails, so the folder isn't left as it was
    /// listed before the change.
    pub(crate) reread: bool,
    /// How many times the user has picked a selection (a click, an arrow, ⌘A, Select by
    /// Pattern…): a reload asked for before the latest pick keeps it ([`Pending::picks`]).
    pub picks: u64,
    pub(crate) anchor: usize,
    /// Typed filter. Starting with `/`, `~` (or a drive on Windows) it's a path to go to instead.
    pub filter: String,
    /// The go-to field is open (⌘L, or a path typed), even once Backspace has emptied it: then
    /// more Backspaces stay in it rather than going to the parent folder. Esc, Enter on a path,
    /// typing a filter or going anywhere closes it.
    pub goto: bool,
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
    /// List, icons, columns or tiles (this tab's; `TabMemo::view` while it's hidden).
    pub view: crate::grid::ViewMode,
    /// The icon view's columns as last laid out, written by the view, for the arrows' 2-D moves
    /// and for revealing the cursor's row (geometry, not file data).
    pub(crate) grid_cols: Rc<Cell<usize>>,
    /// The list's width as last laid out, written by the view: how many columns a new icon
    /// size or a switch to the icon view gives, before it's drawn.
    pub(crate) list_w: Rc<Cell<f32>>,
    /// Where the list's top-left corner was last laid out (window points), written by the view:
    /// where "Column Options…" opens the header's menu, just under the header.
    pub(crate) list_at: Rc<Cell<[f32; 2]>>,
    /// The icon view's thumbnails for the cells on screen (`tiles.rs`). Dropping it cancels
    /// what's still being decoded.
    pub tiles: Option<crate::tiles::Tiles>,
    /// The grid rows Rune last said were visible, while the grid is on screen: where thumbnails
    /// are asked for again when the folder or the icon size changes without Rune reporting new
    /// rows (a reload keeps every row's key).
    pub(crate) grid_rows: Option<std::ops::Range<usize>>,
    /// The listing's version and the icon's pixels thumbnails were last asked for at.
    pub(crate) tiles_for: Option<(u64, u32)>,
    /// The column view's other columns, while this tab shows as columns (`columns.rs`); this
    /// folder is the active column. Dropping it stops their reads.
    pub cols: Option<Box<crate::columns::Columns>>,
    /// The width the columns have, as last laid out (geometry, written by the view).
    pub(crate) cols_w: Rc<Cell<f32>>,
    /// Showing as columns: the number of the column element this folder is drawn as
    /// (`columns::next_uid`). It moves with the listing when a column becomes the active one
    /// and back, so a column keeps its element, its scroll and any gesture in it.
    pub(crate) col_uid: Option<u64>,
    /// A column made the active folder, landing (`State::adopt_column`): for its request only.
    pub(crate) adopted: Option<Adopted>,
    /// The free space last read for a folder (shown while it's the one on screen), and where
    /// the answer to the newest query lands ([`Folder::ask_free`], `volinfo.rs`).
    pub(crate) free: Option<(PathBuf, Option<u64>)>,
    pub(crate) free_inbox: Arc<crate::volinfo::Inbox>,
}

/// What a column made the active folder brings besides its listing.
pub(crate) struct Adopted {
    /// The request its listing lands as.
    pub req: u64,
    /// The column element it was drawn as, which the folder is drawn as from then on.
    pub uid: u64,
    /// Its rows as the column showed them (indices into its entries, and their version), when
    /// they're what the folder shows too (same sort and hidden-file rule): kept rather than
    /// worked out again, under the same version, so Rune looks at no key.
    pub rows: Option<(Rows, u64)>,
}

impl Drop for Folder {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

impl Folder {
    pub fn new(id: u64, cwd: PathBuf, sort: Sort, show_hidden: bool, waker: Option<rune::Waker>) -> Folder {
        let (loader, free_inbox) = (Latest::default(), Arc::new(crate::volinfo::Inbox::default()));
        if let Some(w) = waker {
            free_inbox.set_waker(w.clone());
            loader.set_waker(w);
        }
        Folder {
            id,
            cwd,
            dir_id: None,
            entries: Arc::default(),
            visible: Arc::default(),
            sel: 0,
            marked: Marked::default(),
            none: false,
            carry: Bits::default(),
            labels: HashMap::new(),
            reread: false,
            picks: 0,
            anchor: 0,
            filter: String::new(),
            goto: false,
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
            view: crate::grid::ViewMode::List,
            grid_cols: Rc::new(Cell::new(1)),
            list_w: Rc::new(Cell::new(0.0)),
            list_at: Rc::new(Cell::new([0.0, 0.0])),
            tiles: None,
            grid_rows: None,
            tiles_for: None,
            cols: None,
            cols_w: Rc::new(Cell::new(0.0)),
            col_uid: None,
            adopted: None,
            free: None,
            free_inbox,
        }
    }

    /// Stops the read in flight (its answer could only be stale now) and arms a flag for the
    /// next one.
    pub(crate) fn stop_read(&mut self) -> Arc<AtomicBool> {
        self.stop.store(true, Relaxed);
        self.stop = Arc::default();
        self.stop.clone()
    }

    /// The list's element key: the pane's, or as columns the column element's it's drawn as.
    pub fn list_key(&self) -> String {
        match self.col_uid {
            Some(u) => format!("col-{u}"),
            None => format!("list-{}", self.id),
        }
    }

    pub fn rows_key(&self) -> String {
        match self.col_uid {
            Some(u) => format!("colrows-{u}"),
            None => format!("rows-{}", self.id),
        }
    }

    /// What a command on "the item" acts on: the cursor's item while it's selected, else the
    /// first selected item (the cursor may have been ⌘-clicked off); `None` with nothing
    /// selected. Never an item that isn't selected.
    pub fn selected(&self) -> Option<&Entry> {
        self.selected_row().and_then(|r| self.visible.get(r)).map(|&i| &self.entries[i as usize])
    }

    /// [`Folder::selected`]'s row.
    pub fn selected_row(&self) -> Option<usize> {
        if self.marked.is_empty() {
            return (!self.none && self.sel < self.visible.len()).then_some(self.sel);
        }
        if self.marked.contains(self.sel) { Some(self.sel) } else { self.marked.iter().next() }
    }

    /// The index in the listing of the item named `os`: `at` when that's it (no look through
    /// a big listing), else found.
    pub fn entry_at(&self, os: &OsStr, at: Option<usize>) -> Option<usize> {
        at.filter(|&i| self.entries.get(i).is_some_and(|e| e.os == os)).or_else(|| self.entries.iter().position(|e| e.os == os))
    }

    /// The cursor's item, selected or not: where the cursor is.
    pub fn cursor(&self) -> Option<&Entry> {
        self.visible.get(self.sel).map(|&i| &self.entries[i as usize])
    }

    /// Whether row `i` (index into `visible`) is part of the selection.
    pub fn is_selected(&self, i: usize) -> bool {
        if self.marked.is_empty() { !self.none && i == self.sel } else { self.marked.contains(i) }
    }

    /// The selection as entries, in display order.
    pub fn targets(&self) -> Vec<&Entry> {
        if self.marked.is_empty() {
            return self.selected().into_iter().collect();
        }
        self.marked.iter().filter_map(|r| self.visible.get(r)).map(|&i| &self.entries[i as usize]).collect()
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

    /// Where a drop on this folder's background goes: the folder, as its listing knew it.
    pub fn dest(&self) -> crate::ops::Dest {
        crate::ops::Dest { path: self.cwd.clone(), id: self.dir_id.map(crate::ops::DestId::Folder) }
    }

    /// Where a drop on folder row `e` goes: into it, as this listing read it.
    pub fn row_dest(&self, e: &Entry) -> crate::ops::Dest {
        crate::ops::Dest { path: self.cwd.join(&e.os), id: e.id.map(crate::ops::DestId::Item) }
    }

    /// Whether this folder is the one at `path` (with identity `id`): the same identity when
    /// both are known (a symlink and its target are one folder), else the same path.
    pub fn shows(&self, path: &Path, id: Option<crate::noclobber::DirId>) -> bool {
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
        self.request_chosen(path, nav, select, None, Ids::default(), rename);
    }

    /// [`Folder::request`] with the cursor's item apart from the selection
    /// ([`Pending::cursor`]), and the items' identities when known ([`Pending::ids`]).
    pub(crate) fn request_chosen(
        &mut self,
        path: PathBuf,
        nav: Nav,
        select: Vec<OsString>,
        cursor: Option<OsString>,
        ids: Ids,
        rename: bool,
    ) {
        let mut p = Pending { req: 0, path: PathBuf::new(), nav, select, cursor, none: false, rename, scroll: 0.0, picks: 0, ids };
        let pick = p.pick();
        let ids = std::mem::take(&mut p.ids);
        self.request_pick(path, nav, p.select, p.cursor, rename, pick, false);
        if let Some(p) = &mut self.pending {
            p.ids = ids;
        }
    }

    /// A read of the folder again keeping the selection on screen as it is: handed to the
    /// worker without copying a name ([`Kept`]), which finds it in the new listing.
    pub(crate) fn request_kept(&mut self, nav: Nav, rename: bool) {
        let (kept, none) = (self.kept(), self.selected().is_none());
        self.request_pick(self.cwd.clone(), nav, Vec::new(), None, rename, Pick::Kept(kept), none);
    }

    /// The selection on screen, for a worker ([`Kept`]): two shared pointers and the selected
    /// rows' bits.
    pub(crate) fn kept(&self) -> Kept {
        Kept { entries: self.entries.clone(), rows: self.visible.clone(), marked: self.marked.clone(), sel: self.sel, none: self.none }
    }

    #[allow(clippy::too_many_arguments)]
    fn request_pick(
        &mut self,
        path: PathBuf,
        nav: Nav,
        select: Vec<OsString>,
        cursor: Option<OsString>,
        rename: bool,
        pick: Pick,
        none: bool,
    ) {
        // Going anywhere ends a search (its walk stops), and the thumbnails being decoded. The
        // go-to field stays as it is (a reload keeps it; a navigation that lands clears its text
        // and closes it together).
        self.search = None;
        self.tiles = None;
        // A column being made active (`State::adopt_column`) is superseded.
        self.adopted = None;
        self.reread = path != self.cwd && (self.reread || self.pending.as_ref().is_some_and(|p| p.path == self.cwd));
        let (sort, req, stop) = (self.sort, next_request(), self.stop_read());
        let picks = self.picks;
        self.pending =
            Some(Pending { req, path: path.clone(), nav, select, cursor, none, rename, scroll: 0.0, picks, ids: Ids::default() });
        // An answer about the folder's volume asked before isn't wanted: the new listing asks
        // again when it lands (`State::land`), with the volume its identity names.
        self.free_inbox.forget();
        self.loader.request_opt(path, move |p| Answer::read(req, sort, p, pick, &stop));
    }

    /// Asks for a fresh read of the folder as it is ([`Nav::Show`]), waiting here at most `wait`
    /// for it: a local folder lands in the same frame, a slow one later through `poll`, with
    /// the path and an empty list on screen until then.
    pub(crate) fn request_now(&mut self, wait: std::time::Duration) -> Option<Answer> {
        let req = self.pending.as_ref()?.req;
        let (path, sort, stop) = (self.cwd.clone(), self.sort, self.stop_read());
        let pick = self.pending.as_ref().map_or(Pick::Nothing, Pending::pick);
        crate::bounded::read_waiting(&self.loader, wait, move || Answer::read(req, sort, path, pick, &stop))
    }

    /// This tab's lightweight state, to keep while it's hidden.
    pub fn memo(&self) -> TabMemo {
        // Searching: the plain listing's state, and the query to search for again.
        if let Some(s) = &self.search {
            return TabMemo { search: Some(s.edit.text.clone()).filter(|q| !q.trim().is_empty()), view: self.view, ..s.memo.clone() };
        }
        // On its way somewhere (a folder opened, a request from another app) when it's hidden:
        // keep where it's going and what it was going to select, with the history as landing
        // would leave it, so the hidden tab shows that folder (independent of disk timing).
        if let Some(p) = self.pending.as_ref().filter(|p| matches!(p.nav, Nav::New | Nav::Back | Nav::Forward | Nav::Fresh)) {
            let (mut back, mut fwd) = (self.back.clone(), self.fwd.clone());
            match p.nav {
                Nav::New => {
                    back.push(self.cwd.clone());
                    fwd.clear();
                }
                Nav::Back => {
                    back.pop();
                    fwd.push(self.cwd.clone());
                }
                Nav::Forward => {
                    fwd.pop();
                    back.push(self.cwd.clone());
                }
                _ => {
                    back.clear();
                    fwd.clear();
                }
            }
            return TabMemo {
                path: p.path.clone(),
                back,
                fwd,
                select: p.select.clone(),
                cursor: p.cursor.clone(),
                none: p.none,
                scroll: 0.0,
                filter: String::new(),
                goto: false,
                sort: self.sort,
                sort_set: self.sort_set,
                search: None,
                view: self.view,
                ids: p.ids.clone(),
            };
        }
        // Hidden again before its read landed: keep what it was going to restore.
        if let Some(p) = self.pending.as_ref().filter(|p| p.nav == Nav::Show) {
            return TabMemo {
                path: self.cwd.clone(),
                back: self.back.clone(),
                fwd: self.fwd.clone(),
                select: p.select.clone(),
                cursor: p.cursor.clone(),
                none: p.none,
                scroll: p.scroll,
                filter: self.filter.clone(),
                goto: self.goto,
                sort: self.sort,
                sort_set: self.sort_set,
                search: None,
                view: self.view,
                ids: p.ids.clone(),
            };
        }
        // The selection exactly (in display order, so it reads back the same), and the cursor
        // apart: it may be off the selection. With their identities, so an item renamed while
        // the tab is hidden is found under its new name.
        let cursor = self.cursor();
        let items: Vec<&Entry> = if self.marked.is_empty() { cursor.filter(|_| !self.none).into_iter().collect() } else { self.targets() };
        let ids = Ids { select: items.iter().map(|e| e.id).collect(), cursor: cursor.and_then(|e| e.id) };
        let select: Vec<OsString> = items.iter().map(|e| e.os.clone()).collect();
        let cursor = cursor.map(|e| e.os.clone());
        TabMemo {
            path: self.cwd.clone(),
            back: self.back.clone(),
            fwd: self.fwd.clone(),
            none: select.is_empty(),
            select,
            cursor,
            scroll: self.scroll.get(),
            filter: self.filter.clone(),
            goto: self.goto,
            sort: self.sort,
            sort_set: self.sort_set,
            search: None,
            view: self.view,
            ids,
        }
    }

    /// Shows a hidden tab: its path, history, filter and sort at once, an empty list until its
    /// fresh read lands (`request_now`), then its selection and scroll.
    pub fn show(&mut self, m: TabMemo) {
        // The tab that was here is hidden now: its read or search, if any, stops, and an answer
        // about its volume isn't wanted.
        self.stop_read();
        self.free_inbox.forget();
        self.search = None;
        self.tiles = None;
        self.grid_rows = None;
        self.tiles_for = None;
        self.cols = None;
        self.col_uid = None;
        self.adopted = None;
        self.entries = Arc::default();
        self.visible = Arc::default();
        self.labels.clear();
        self.reread = false;
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
        self.goto = m.goto;
        self.sort = m.sort;
        self.sort_set = m.sort_set;
        self.view = m.view;
        self.scroll.set(m.scroll);
        self.pending = Some(Pending {
            req: next_request(),
            path: self.cwd.clone(),
            nav: Nav::Show,
            select: m.select,
            cursor: m.cursor,
            none: m.none,
            rename: false,
            scroll: m.scroll,
            picks: self.picks,
            ids: m.ids,
        });
    }

    /// The same folder as `other`, as a second pane opened on it: its listing as on screen,
    /// the cursor on the same name, no history or filter.
    pub fn same_as(id: u64, other: &Folder, waker: Option<rune::Waker>) -> Folder {
        let mut f = Folder::new(id, other.cwd.clone(), other.sort, other.show_hidden, waker);
        f.sort_set = other.sort_set;
        f.view = other.view;
        f.dir_id = other.dir_id;
        f.free = other.free.clone();
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
    /// otherwise the first name starting with the filter, else the first row. The selection
    /// stays on the items it was on, those still shown.
    pub fn refilter(&mut self, select: Option<&OsStr>) {
        self.carry_marks();
        self.refilter_carried(select);
    }

    /// Puts the selection on its entries (`Entry::sel`) for the rows to be made again:
    /// [`Folder::refilter_carried`] (also after a sort, which moves entries) takes it back.
    pub(crate) fn carry_marks(&mut self) {
        for r in self.marked.iter() {
            if let Some(&i) = self.visible.get(r) {
                self.carry.set(i as usize);
            }
        }
        // The cursor alone selected rides the same way: it's as much a selection.
        if self.marked.is_empty()
            && !self.none
            && let Some(&i) = self.visible.get(self.sel)
        {
            self.carry.set(i as usize);
        }
        self.marked.clear();
    }

    /// Takes the carried selection back onto the same rows ([`Folder::carry_marks`] with the
    /// rows unchanged).
    pub(crate) fn restore_marks(&mut self) {
        for r in 0..self.visible.len() {
            if self.carry.get(self.visible[r] as usize) {
                self.marked.set(r, true, &self.visible);
            }
        }
        self.carry.clear();
    }

    /// [`Folder::refilter`] with the selection already on the entries ([`Folder::carry_marks`],
    /// or a listing marked on its worker): the rows shown, selected as their entries say.
    /// What isn't shown any more (hidden, filtered out) isn't selected any more: the selection
    /// is what's on screen, as what an action acts on is.
    pub(crate) fn refilter_carried(&mut self, select: Option<&OsStr>) -> bool {
        let q = if self.path_mode() { String::new() } else { self.filter.to_lowercase() };
        let hidden = self.show_hidden;
        // A search's list holds only the results it has taken in so far (`search.rs`).
        let n = self.search.as_ref().map_or(self.entries.len(), |s| s.upto.min(self.entries.len()));
        self.visible = Arc::new(Rows::of(&self.entries, 0..n, |e| (hidden || !e.hidden) && (q.is_empty() || e.lower.contains(&q))));
        self.marked.clear();
        for r in 0..self.visible.len() {
            if self.carry.get(self.visible[r] as usize) {
                self.marked.set(r, true, &self.visible);
            }
        }
        let carried = self.carry.count();
        self.carry.clear();
        // Items deselected because they aren't shown any more: the selection changed, so a
        // reload asked for before (with the old selection) doesn't bring them back.
        if self.marked.len() < carried {
            self.picks += 1;
        }
        let by_name = select.and_then(|n| self.visible.iter().position(|&i| self.entries[i as usize].os == n));
        let by_prefix =
            || (!q.is_empty()).then(|| self.visible.iter().position(|&i| self.entries[i as usize].lower.starts_with(&q))).flatten();
        self.sel = by_name.or_else(by_prefix).unwrap_or(0);
        // The cursor's item alone is the cursor alone selected; what was selected and is all
        // gone leaves nothing selected (never a row put in its place); with nothing carried the
        // caller says (a listing landing).
        let survived = !self.marked.is_empty();
        if self.marked.len() == 1 && self.marked.contains(self.sel) {
            self.marked.clear();
            self.none = false;
        } else if !self.marked.is_empty() {
            self.none = false;
        } else if carried > 0 {
            self.none = true;
        }
        self.bump();
        survived
    }

    /// `visible` changed: virtual rows look at every key again.
    pub(crate) fn bump(&mut self) {
        self.version = next_version();
    }

    /// The selection by name, exactly, in display order (with nothing marked, the cursor's
    /// item), and the cursor's item apart (it may be off the selection): what a hidden tab
    /// keeps, and what a reload puts back ([`Pending::select`], [`Pending::cursor`]).
    pub fn selection_names(&self) -> (Vec<OsString>, Option<OsString>) {
        let cursor = self.cursor_name();
        let items = if self.marked.is_empty() {
            cursor.clone().into_iter().filter(|_| !self.none).collect()
        } else {
            self.targets().into_iter().map(|e| e.os.clone()).collect()
        };
        (items, cursor)
    }

    /// The cursor's item's name, selected or not.
    pub fn cursor_name(&self) -> Option<OsString> {
        self.cursor().map(|e| e.os.clone())
    }

    pub fn set_sort(&mut self, key: SortKey) {
        self.sort_set = true;
        // Search results are in the order found until a header is clicked: the first click
        // sorts by it, the way a new column does.
        let first = self.search.as_mut().is_some_and(|s| !std::mem::replace(&mut s.sorted, true));
        let dirs_first = self.sort.dirs_first;
        self.sort = if self.sort.key == key && !first {
            Sort { key, desc: !self.sort.desc, dirs_first }
        } else {
            // Newest and largest first feel right; names and kinds read A→Z.
            Sort { key, desc: matches!(key, SortKey::Size | SortKey::Modified | SortKey::Created), dirs_first }
        };
        self.resort();
    }

    /// Folders First turned on or off: the listing (or a search's sorted results) sorted again,
    /// keeping the cursor; results still in the order found keep it, and take it when sorted.
    pub fn set_dirs_first(&mut self, on: bool) {
        if let Some(s) = self.search.as_mut() {
            s.memo.sort.dirs_first = on;
            if !s.sorted {
                self.sort.dirs_first = on;
                return;
            }
        }
        if self.sort.dirs_first != on {
            self.sort.dirs_first = on;
            self.resort();
        }
    }

    /// Tests: selects the items named (with their sizes, as a click would).
    #[cfg(test)]
    pub fn mark_names(&mut self, names: &[&str]) {
        self.marked.clear();
        for r in 0..self.visible.len() {
            if names.iter().any(|n| self.entries[self.visible[r] as usize].os == *n) {
                self.marked.set(r, true, &self.visible);
            }
        }
    }

    /// What the selection holds, for the status bar, while two or more items are selected:
    /// how many, the files' total size, and how many folders (whose size isn't known: nothing
    /// walks them). Kept as the selection changes (`marked.rs`): no work here.
    pub fn picked_size(&self) -> Option<Picked> {
        (self.marked.len() >= 2).then(|| self.marked.picked())
    }

    /// Asks for the free space of the volume the folder on screen is on (`volinfo.rs`: at most
    /// four threads of its own, never the listings' workers): when its listing lands and after a file
    /// operation, never on a timer. A newer query supersedes this folder's older one; the
    /// answer lands in `State::poll`.
    pub(crate) fn ask_free(&mut self) {
        let device = self.dir_id.map(|d| d.volume());
        match crate::volinfo::ask(&self.free_inbox, device, self.cwd.clone()) {
            crate::volinfo::Started::Yes => {}
            // Every reader is stuck, or the volume isn't known: no free space shown (not a
            // figure that may be out of date).
            crate::volinfo::Started::Waiting | crate::volinfo::Started::Unknown => self.free = None,
        }
    }

    /// The free space read for the folder on screen, if it's in.
    pub fn free(&self) -> Option<u64> {
        self.free.as_ref().filter(|(p, _)| *p == self.cwd).and_then(|(_, n)| *n)
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
        // The selection moves with its entries through the sort.
        self.carry_marks();
        let sort = self.sort;
        sort_carrying(Arc::make_mut(&mut self.entries), &mut self.carry, sort);
        // A search shows only the results it has taken in so far: whatever is selected (and the
        // cursor's item) is taken in too, wherever the sort put it, so nothing selected is lost
        // past the end and no other row takes its place.
        if let Some(s) = self.search.as_mut() {
            let cursor = keep.as_ref().and_then(|k| self.entries.iter().rposition(|e| e.os == *k));
            let last = self.carry.last().max(cursor);
            s.upto = s.upto.max(last.map_or(0, |l| l + 1)).min(self.entries.len());
        }
        self.refilter_carried(keep.as_deref());
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
        self.picks += 1;
        self.none = false;
        if !self.visible.is_empty() {
            self.sel = i.min(self.visible.len() - 1);
            self.anchor = self.sel;
            self.marked.clear();
        }
    }

    /// ⌘-click: add or remove this row.
    pub fn toggle_mark(&mut self, i: usize) {
        self.picks += 1;
        if i >= self.visible.len() {
            return;
        }
        // The cursor alone counts as selected (unless nothing is): ⌘ adds to it.
        if self.marked.is_empty() && !self.none {
            self.marked.set(self.sel, true, &self.visible);
        }
        self.marked.toggle(i, &self.visible);
        // Toggled down to nothing: nothing selected (the cursor's row was just deselected).
        self.none = self.marked.is_empty();
        self.sel = i;
        self.anchor = i;
    }

    /// ⇧-click or ⇧-arrow: everything from the anchor to row `i`.
    pub fn extend_to(&mut self, i: usize) {
        self.picks += 1;
        if self.visible.is_empty() {
            return;
        }
        let i = i.min(self.visible.len() - 1);
        let (a, b) = (self.anchor.min(i), self.anchor.max(i));
        self.none = false;
        // Grown or shrunk from the range it was: only the rows between change.
        self.marked.select_range(a, b, &self.visible);
        self.sel = i;
    }

    pub fn select_all(&mut self) {
        self.picks += 1;
        self.none = self.visible.is_empty();
        self.marked.select_range(0, self.visible.len().saturating_sub(1), &self.visible);
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
        if self.view.is_grid() {
            // The grid's rows are keyed by their first item.
            let cols = self.grid_cols.get().max(1);
            if let Some(&e) = self.visible.get(self.sel / cols * cols) {
                tx.scroll_into_view_in(self.rows_key(), self.entries[e as usize].key.clone());
            }
            return;
        }
        if let Some(e) = self.cursor() {
            tx.scroll_into_view_in(self.rows_key(), e.key.clone());
        }
    }

    /// A grid view (icons at `size`, or tiles) is about to show: its columns from the list's
    /// width, so the cursor's row can be revealed in this frame.
    pub(crate) fn regrid(&self, size: crate::grid::IconSize) {
        if self.list_w.get() > 0.0
            && let Some(g) = self.view.geom(size, self.list_w.get())
        {
            self.grid_cols.set(g.cols);
        }
    }

    /// An arrow in a grid view: `dx` items along, or `dy` rows up or down (`grid::step`).
    pub fn step_grid(&mut self, dx: isize, dy: isize, extend: bool, tx: &mut Tx) {
        if self.visible.is_empty() {
            return;
        }
        let to = crate::grid::step(self.sel, self.grid_cols.get(), self.visible.len(), dx, dy);
        if extend {
            self.extend_to(to);
        } else {
            self.select(to);
        }
        self.reveal(tx);
    }

    /// The item `old` is being renamed `new` on the worker: its row shows the new name at once,
    /// but it's still `old` until the outcome lands, and every action on it waits: a rename
    /// that fails (the name was taken) never points the row at the other item. A label beside
    /// the listing: nothing in it changes. `at`: its index in the listing, if known.
    pub fn rename_pending(&mut self, old: &OsStr, new: &str, at: Option<usize>) {
        let id = self.entry_at(old, at).and_then(|i| self.entries[i].id);
        self.labels.insert(old.to_os_string(), Label { name: new.to_string(), at, waiting: true, id });
        self.bump();
    }

    /// The name row `e` shows: its label's ([`Folder::labels`]), or its own.
    pub fn label<'a>(&'a self, e: &'a Entry) -> &'a str {
        self.labels.get(&e.os).map_or(&e.name, |l| &l.name)
    }

    /// Whether row `e` shows a label: nothing acts on it until the folder is read again.
    pub fn labelled(&self, e: &Entry) -> bool {
        !self.labels.is_empty() && self.labels.contains_key(&e.os)
    }

    /// Whether an action on the selection has to wait: one of its items shows a label (its
    /// rename on its way, or landed while the listing was shared) until the folder is read
    /// again. Checks nothing without labels.
    pub fn updating(&self) -> bool {
        !self.labels.is_empty() && self.targets().iter().any(|e| self.labelled(e))
    }

    /// [`Folder::updating`] for a drag out of row `i` (the selection, if the row is in it).
    pub fn drag_updating(&self, i: usize) -> bool {
        if self.labels.is_empty() {
            return false;
        }
        if self.is_selected(i) {
            return self.updating();
        }
        self.visible.get(i).is_some_and(|&e| self.labelled(&self.entries[e as usize]))
    }

    /// Whether a reload's worker or the other pane holds the listing (changing it in place
    /// would copy it).
    fn shared(&self) -> bool {
        Arc::strong_count(&self.entries) > 1
    }

    /// An inline rename's outcome landed (`moved`: the items' old and new paths, matched by
    /// name, so through whichever path this pane reached the folder by). A row it renamed takes
    /// its new name in the listing itself while nobody else holds it; while a reload's worker
    /// or the other pane does, its label stays (actions on it still wait) until the read the
    /// change asks for lands. A rename that failed loses its label. Returns the renames it took
    /// in, for [`Folder::renamed_paths`] to skip.
    pub fn land_pending(&mut self, moved: &[(PathBuf, PathBuf)]) -> Vec<(PathBuf, PathBuf)> {
        let mut done = Vec::new();
        if !self.labels.values().any(|l| l.waiting) {
            return done;
        }
        let labels = std::mem::take(&mut self.labels);
        self.bump();
        for (old, mut l) in labels {
            if !l.waiting {
                self.labels.insert(old, l);
                continue;
            }
            let Some(m) = moved.iter().find(|(f, t)| f.file_name() == Some(old.as_os_str()) && t.file_name() == Some(OsStr::new(&l.name)))
            else {
                continue;
            };
            done.push(m.clone());
            if self.renamed(&old, OsStr::new(&l.name), l.at) {
                l.waiting = false;
                self.labels.insert(old, l);
            }
        }
        done
    }

    /// The item `old` was renamed `new` on disk: in the listing itself while nobody else holds
    /// it (one entry, found by its index when `at` is it). True when it can't be (the listing
    /// is shared): its label stays until the read that follows. Nothing to do when the listing
    /// was read since (no `old`, or `old` a newcomer while the disk has the item at `new`).
    fn renamed(&mut self, old: &OsStr, new: &OsStr, at: Option<usize>) -> bool {
        if self.shared() {
            return true;
        }
        let Some(ix) = self.entry_at(old, at) else { return false };
        // Too big to look through for a stale entry under the new name within a frame: renamed
        // by its row, and every row of that name waits (a label) for the read that follows.
        if self.entries.len() > SCAN_MAX {
            let name = new.to_string_lossy().into_owned();
            self.rename_entry(ix, new);
            self.labels.insert(new.to_os_string(), Label { name, at: Some(ix), waiting: false, id: None });
            return false;
        }
        match self.entries.iter().position(|o| o.os == new) {
            None => self.rename_entry(ix, new),
            // Listed under both names: the entry under the new one is stale (gone before the
            // rename found the name free) when the disk has this item there.
            Some(j) => {
                let id = self.entries[ix].id;
                if id.is_some() && fs::ident_at(&self.cwd.join(new)) == id {
                    self.rename_listed(&[(ix, new.to_os_string())], &[(j, ix)]);
                }
            }
        }
        false
    }

    /// Renames entries in the listing itself (nobody else holds it): `renames`, by index, where
    /// each entry `stale.0` was listed under a new name but gone before the rename found it free:
    /// entry `stale.1`, renamed to it, takes its place (its row, and its selection when it's the
    /// same kind of item, as a read of the folder would have it). Without stale entries, only the
    /// entries renamed change (the rows too, if one is hidden or filtered out now, or back).
    fn rename_listed(&mut self, renames: &[(usize, OsString)], stale: &[(usize, usize)]) {
        if stale.is_empty() {
            for (ix, new) in renames {
                self.rename_entry(*ix, new);
            }
            return;
        }
        let names: HashMap<usize, &OsString> = renames.iter().map(|(i, n)| (*i, n)).collect();
        let gone: HashMap<usize, usize> = stale.iter().copied().collect();
        let new_of = |i: usize| names.get(&i).map(|n| (*n).clone());
        let at = self.visible.get(self.sel).map(|&e| e as usize);
        let keep = match at {
            Some(c) => new_of(gone.get(&c).copied().unwrap_or(c)).or_else(|| self.cursor_name()),
            None => None,
        };
        self.carry_marks();
        let mut old = std::mem::take(&mut self.carry);
        // The item renamed takes the stale row's selection when it's the same kind of thing, as
        // a read of the folder would (`find_named`).
        let kind = |e: &Entry| (e.is_dir, e.is_link);
        for &(j, heir) in stale {
            if old.get(j) && kind(&self.entries[j]) == kind(&self.entries[heir]) {
                old.set(heir);
            }
        }
        let (mut carry, mut k) = (Bits::default(), 0);
        let entries = Arc::make_mut(&mut self.entries);
        let mut kept = Vec::with_capacity(entries.len());
        for (i, mut e) in std::mem::take(entries).into_iter().enumerate() {
            if gone.contains_key(&i) {
                continue;
            }
            if old.get(i) {
                carry.set(k);
            }
            if let Some(new) = new_of(i) {
                set_name(&mut e, &new);
            }
            kept.push(e);
            k += 1;
        }
        *entries = kept;
        self.carry = carry;
        self.refilter_carried(keep.as_deref());
    }

    /// Entry `ix` is called `new` now, in the listing itself (nobody else holds it).
    fn rename_entry(&mut self, ix: usize, new: &OsStr) {
        let shown = |f: &Folder, e: &Entry| {
            let q = if f.path_mode() { String::new() } else { f.filter.to_lowercase() };
            (f.show_hidden || !e.hidden) && (q.is_empty() || e.lower.contains(&q))
        };
        let before = shown(self, &self.entries[ix]);
        set_name(&mut Arc::make_mut(&mut self.entries)[ix], new);
        if shown(self, &self.entries[ix]) == before {
            // The same rows: its row shows the new name (its key changed).
            self.bump();
            return;
        }
        let keep = if self.visible.get(self.sel) == Some(&(ix as u32)) { Some(new.to_os_string()) } else { self.cursor_name() };
        self.refilter(keep.as_deref());
    }

    /// A new listing landed: labels of renames that landed go (it has them), and so does every
    /// label when it's another folder; an inline rename still on its way keeps its label on a
    /// row still called what it was. Looks through the listing only while a rename is on its way.
    pub(crate) fn listing_replaced(&mut self, moved: bool) {
        if self.labels.is_empty() {
            return;
        }
        if moved {
            self.labels.clear();
            return;
        }
        let entries = self.entries.clone();
        let same = |l: &Label, e: &Entry| l.id.is_none() || e.id.is_none() || e.id == l.id;
        self.labels.retain(|old, l| l.waiting && entries.iter().any(|e| e.os == *old && same(l, e)));
    }

    /// [`Folder::renamed_paths`] in a folder's own listing (nobody else holds it), by the items'
    /// names (`map`: old to new). An item listed under both names was read since the rename
    /// (the one under the old name is a newcomer: left alone), or the entry under the new name
    /// is stale (gone before the rename found the name free: the item takes its place); the
    /// disk says which. A few renames are found by comparing names, many through a set: one
    /// pass either way.
    fn rename_names(&mut self, map: &HashMap<PathBuf, PathBuf>) {
        let pairs: Vec<(&OsStr, &OsStr)> = map.iter().map(|(f, t)| (f.as_os_str(), t.as_os_str())).collect();
        let (mut from, mut to) = (vec![None; pairs.len()], vec![None; pairs.len()]);
        if pairs.len() <= 16 {
            for (i, e) in self.entries.iter().enumerate() {
                for (p, (f, t)) in pairs.iter().enumerate() {
                    if e.os == *f {
                        from[p] = Some(i);
                    } else if e.os == *t {
                        to[p] = Some(i);
                    }
                }
            }
        } else {
            let ix: HashMap<&OsStr, (usize, bool)> =
                pairs.iter().enumerate().flat_map(|(p, (f, t))| [(*f, (p, true)), (*t, (p, false))]).collect();
            for (i, e) in self.entries.iter().enumerate() {
                match ix.get(e.os.as_os_str()) {
                    Some(&(p, true)) => from[p] = Some(i),
                    Some(&(p, false)) => to[p] = Some(i),
                    None => {}
                }
            }
        }
        let (mut renames, mut stale) = (Vec::new(), Vec::new());
        for (p, (_, t)) in pairs.iter().enumerate() {
            match (from[p], to[p]) {
                (Some(i), None) => renames.push((i, t.to_os_string())),
                (Some(i), Some(j)) => {
                    let id = self.entries[i].id;
                    if id.is_some() && fs::ident_at(&self.cwd.join(t)) == id {
                        renames.push((i, t.to_os_string()));
                        stale.push((j, i));
                    }
                }
                (None, _) => {}
            }
        }
        self.rename_listed(&renames, &stale);
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
        if !searching {
            // A reload's worker or the other pane holds the listing: it isn't changed (a copy of
            // a big one would hold up the window), and nothing translates it: each row renamed
            // shows its new name as a label, and every action on it waits, until the read the
            // change asks for (`State::refresh`, superseding any read on its way) lands. The
            // same for a listing too big to look through within a frame.
            if self.shared() || self.entries.len() > SCAN_MAX {
                for (f, t) in map {
                    let name = t.to_string_lossy().into_owned();
                    self.labels.insert(f.into_os_string(), Label { name, at: None, waiting: false, id: None });
                }
                self.bump();
                return;
            }
            return self.rename_names(&map);
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
        for e in Arc::make_mut(&mut self.entries).iter_mut() {
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
            e.os = new.into_os_string();
        }
        self.refilter(keep.as_deref());
    }

    /// Rows whose identity is one an operation's own change replaced (`ops::Outcome::reidentified`:
    /// where identity is told by ctime alone) take the new one, whatever their name: the item
    /// renamed, and other names for the same inode (hard links). A row whose identity was already
    /// stale (changed by another app first) matches nothing and stays so.
    pub fn reidentify(&mut self, pairs: &[(crate::fs::Ident, crate::fs::Ident)]) {
        if pairs.is_empty() {
            return;
        }
        // A reload's worker holds the listing: the reload brings the identities (until then an
        // action on such an item is refused, as for any item changed since it was read).
        if self.shared() || !self.entries.iter().any(|e| e.id.is_some_and(|id| crate::ops::reidentified(id, pairs) != id)) {
            return;
        }
        for e in Arc::make_mut(&mut self.entries).iter_mut() {
            if let Some(id) = e.id.as_mut() {
                *id = crate::ops::reidentified(*id, pairs);
            }
        }
    }

    pub fn type_filter(&mut self, text: &str) {
        let keep = self.cursor_name();
        self.filter.push_str(text);
        // A path opens the go-to field; anything else is a filter (and closes it).
        self.goto = self.path_mode();
        // Typing chooses again: nothing of the old selection is carried (a filter selects the
        // row it finds; go-to keeps the cursor where it was). A choice like a click: a reload
        // asked for before brings back nothing of what was selected then.
        self.picks += 1;
        self.marked.clear();
        self.none = true;
        self.refilter(if self.path_mode() { keep.as_deref() } else { None });
        // Nothing matches: nothing selected (backspacing doesn't then select a row of its own).
        self.none = self.visible.is_empty();
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

/// Tests: the threads that looked through a listing for a selection's names ([`mark_named`]).
#[cfg(test)]
pub(crate) mod marked_on {
    use std::sync::Mutex;
    use std::thread::ThreadId;

    static ON: Mutex<Vec<ThreadId>> = Mutex::new(Vec::new());

    pub(super) fn note() {
        ON.lock().unwrap_or_else(|e| e.into_inner()).push(std::thread::current().id());
    }

    pub(crate) fn here() -> bool {
        let me = std::thread::current().id();
        ON.lock().unwrap_or_else(|e| e.into_inner()).contains(&me)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A big folder (500,000 items) with the last one renamed inline. While a reload's worker
    /// holds the listing, the rename changes nothing in it (no copy on the window's thread: it
    /// took 136 ms): the row shows the new name as a label, and actions on it wait. Nobody
    /// holding it, the rename lands in the listing itself, found by its row. In a release
    /// build, each step is held to 1 ms.
    #[test]
    fn a_rename_lands_in_a_big_listing_within_a_frame() {
        let n = 500_000;
        let old = OsString::from(format!("{:08}.txt", n - 1));
        let moved = [(PathBuf::from("/x").join(&old), PathBuf::from("/x/renamed.txt"))];
        let mut times = Vec::new();
        for shared in [true, false] {
            let mut f = Folder::new(0, PathBuf::from("/x"), Sort::new(SortKey::Name, false), true, None);
            f.entries = (0..n).map(|i| Entry::test(&format!("{i:08}.txt"), false)).collect::<Vec<_>>().into();
            f.refilter(None);
            f.select(n - 1);
            let at = f.selected_row().map(|r| f.visible[r] as usize);
            let held = shared.then(|| f.kept());
            let listing = Arc::as_ptr(&f.entries);
            let t = std::time::Instant::now();
            f.rename_pending(&old, "renamed.txt", at);
            times.push(t.elapsed());
            assert!(f.updating(), "waits for the rename");
            let t = std::time::Instant::now();
            f.land_pending(&moved);
            times.push(t.elapsed());
            assert!(std::ptr::eq(Arc::as_ptr(&f.entries), listing), "the listing wasn't copied");
            let e = f.selected().unwrap();
            assert_eq!(f.label(e), "renamed.txt");
            if shared {
                assert_eq!(e.os, old, "the listing as it was");
                assert!(f.updating(), "actions wait for the read");
            } else {
                assert_eq!(e.os, "renamed.txt", "in the listing, by its row");
                assert!(f.updating(), "too big to look for a stale entry under its name: waits for the read");
            }
            drop(held);
        }
        // As big as a listing looked through on landing gets: renamed in it, nothing waits.
        let n = SCAN_MAX;
        let mut f = Folder::new(0, PathBuf::from("/x"), Sort::new(SortKey::Name, false), true, None);
        f.entries = (0..n).map(|i| Entry::test(&format!("{i:08}.txt"), false)).collect::<Vec<_>>().into();
        f.refilter(None);
        f.select(n - 1);
        let at = f.selected_row().map(|r| f.visible[r] as usize);
        let old = OsString::from(format!("{:08}.txt", n - 1));
        f.rename_pending(&old, "renamed.txt", at);
        let t = std::time::Instant::now();
        f.land_pending(&[(PathBuf::from("/x").join(&old), PathBuf::from("/x/renamed.txt"))]);
        times.push(t.elapsed());
        assert_eq!(f.selected().map(|e| e.os.clone()), Some("renamed.txt".into()));
        assert!(!f.updating());
        eprintln!(
            "500k shared: shown {:?}, landed {:?}; own: shown {:?}, landed {:?}; 50k own, landed {:?}",
            times[0], times[1], times[2], times[3], times[4]
        );
        if !cfg!(debug_assertions) {
            assert!(times.iter().all(|t| *t < std::time::Duration::from_millis(1)), "{times:?}");
        }
    }

    /// A read carries the selection by identity: an item renamed since is found under its new
    /// name, never a newcomer under its old one; a file saved anew in its place (another file
    /// under its name) is kept; another kind of item under its name (a folder for a file) isn't,
    /// nor what's gone.
    #[test]
    fn a_read_finds_the_selection_by_identity() {
        let d = std::env::temp_dir().join(format!("arbor-find-named-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for n in ["a", "b", "c", "e"] {
            std::fs::write(d.join(n), n).unwrap();
        }
        let id = |n: &str| fs::ident_at(&d.join(n));
        let chosen: Vec<Named> = ["a", "b", "c", "e"].iter().map(|n| (OsString::from(n), id(n))).collect();
        // An editor's save: a new file put in the old one's place.
        std::fs::write(d.join("e.tmp"), "saved").unwrap();
        std::fs::rename(d.join("e.tmp"), d.join("e")).unwrap();
        std::fs::rename(d.join("a"), d.join("renamed")).unwrap();
        std::fs::write(d.join("a"), "newcomer").unwrap();
        std::fs::remove_file(d.join("b")).unwrap();
        std::fs::create_dir(d.join("b")).unwrap();
        std::fs::remove_file(d.join("c")).unwrap();
        let entries: Vec<Entry> =
            ["a", "b", "e", "renamed"].iter().map(|n| Entry { id: id(n), ..Entry::test(n, d.join(n).is_dir()) }).collect();
        let cursor = Some(chosen[0].clone());
        let (select, cursor) = find_named(&entries, chosen, cursor);
        assert_eq!(select, ["renamed", "e"].map(OsString::from), "b is a folder now, c gone");
        assert_eq!(cursor.as_deref(), Some(OsStr::new("renamed")));
        let _ = std::fs::remove_dir_all(&d);
    }
}
