//! Drag-select: a rectangle dragged out from empty space selects every item it touches, live.
//!
//! **Where it starts.** A press on an item acts on the item, as before: a click selects it and a
//! drag drags it (files out to other apps, onto the other pane or the sidebar). An item is a
//! list row's name (the text itself, not the room after it), any part of a row that's already
//! selected (so a selection can be dragged from wherever it's grabbed), or a grid cell's icon
//! and name. A press anywhere else in the list starts a rectangle once it moves 6 pt: the empty
//! part of an unselected row (its ▸, the room after its name, its Kind, Size and Date cells),
//! the empty space below the rows, and the gutters between grid cells. A click there that doesn't move
//! still selects the row; one below the rows (or in a grid's gutter) clears a multiple selection,
//! as Finder's and Explorer's do.
//!
//! **What it selects.** Every item whose row (list view: a row spans the width) or icon cell
//! (grid view) the rectangle touches, worked out from the layout's geometry and the scroll
//! offset, never from the rows that happen to be built (they're virtual). Alone it replaces the
//! selection; with ⇧ it adds to the selection the press started with; with ⌘ (Ctrl elsewhere) it
//! toggles each item it touches against that selection. Esc or losing the pointer puts the
//! selection back.
//!
//! **Past an edge.** Dragged above or below the list, it scrolls, a step on each pointer move,
//! further the further out the pointer is. Rune sends no events while the pointer holds still,
//! and Arbor never runs a timer (NORTHSTAR: no polling), so a still pointer doesn't scroll
//! (RUNE-GAPS.md).

use std::collections::HashSet;
use std::ffi::OsString;
use std::ops::Range;

use crate::folder::Folder;
use crate::grid::Geom;

/// How far the pointer travels before a press in empty space becomes a rectangle, in points.
pub const START: f32 = 6.0;

/// What the rectangle does to the selection it started with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Selects exactly what it touches.
    Replace,
    /// ⇧: adds what it touches.
    Add,
    /// ⌘ (Ctrl elsewhere): flips what it touches.
    Toggle,
}

impl Mode {
    pub fn of(m: rune::Mods) -> Mode {
        if m.cmd {
            Mode::Toggle
        } else if m.shift {
            Mode::Add
        } else {
            Mode::Replace
        }
    }
}

/// How the items are laid out, for finding what a rectangle touches.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Layout {
    /// One item per row, each `row_h` tall, spanning the width.
    List {
        row_h: f32,
    },
    Grid(Geom),
}

/// The items a rectangle touches: whole rows of a list, or a block of rows and columns of a
/// grid (an item `r * cols + c` for each, when there is one).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Hits {
    pub rows: Range<usize>,
    pub cols: Range<usize>,
}

impl Hits {
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() || self.cols.is_empty()
    }
}

impl Layout {
    /// What the rectangle `[x0, y0, x1, y1]` (content points, any corner order) touches, of `n`
    /// items.
    pub fn hits(self, r: [f32; 4], n: usize) -> Hits {
        let (x0, x1) = (r[0].min(r[2]), r[0].max(r[2]));
        let (y0, y1) = (r[1].min(r[3]), r[1].max(r[3]));
        match self {
            Layout::List { row_h } => {
                if y1 < 0.0 || n == 0 {
                    return Hits::default();
                }
                let a = (y0.max(0.0) / row_h).floor() as usize;
                let b = (y1 / row_h).floor() as usize + 1;
                Hits { rows: a.min(n)..b.min(n), cols: 0..1 }
            }
            Layout::Grid(g) => {
                let rows = g.rows(n);
                // Each item is its cell less the inset: find the first and last whose edges
                // the rectangle reaches.
                let span = |lo: f32, hi: f32, pad: f32, size: f32, count: usize| -> Range<usize> {
                    let inner = size - 2.0 * crate::grid::INSET;
                    let start = |k: usize| pad + k as f32 * size + crate::grid::INSET;
                    let mut a = ((lo - pad) / size).floor().max(0.0) as usize;
                    if a < count && start(a) + inner < lo {
                        a += 1;
                    }
                    let mut b = ((hi - pad) / size).floor().max(-1.0);
                    if b >= 0.0 && start(b as usize) > hi {
                        b -= 1.0;
                    }
                    let b = if b < 0.0 { 0 } else { b as usize + 1 };
                    a.min(count)..b.min(count).max(a.min(count))
                };
                Hits { rows: span(y0, y1, 0.0, g.cell[1], rows), cols: span(x0, x1, crate::grid::PAD, g.cell[0], g.cols) }
            }
        }
    }

    /// The indices (into the folder's visible items) of `h`, in display order.
    pub fn each(self, h: &Hits, n: usize) -> impl Iterator<Item = usize> + '_ {
        let cols = match self {
            Layout::List { .. } => 1,
            Layout::Grid(g) => g.cols.max(1),
        };
        let c = h.cols.clone();
        h.rows.clone().flat_map(move |r| c.clone().map(move |c| r * cols + c)).filter(move |&i| i < n)
    }
}

/// A rectangle being dragged out in one pane's list.
pub struct Band {
    pub pane: u64,
    /// The folder's version when it started: a listing that lands meanwhile ends it.
    pub version: u64,
    /// Where it started and where the pointer is, in content points (scrolling included).
    pub start: [f32; 2],
    pub end: [f32; 2],
    pub mode: Mode,
    pub layout: Layout,
    /// The list's viewport in window points (x, y, w, h), as laid out when it started.
    pub view: [f32; 4],
    /// Where the list was scrolled, as far as the band knows (its own scrolling included).
    pub scroll: f32,
    /// It has travelled far enough to be a rectangle.
    pub moved: bool,
    /// What it touched when last applied.
    pub hits: Hits,
    /// What ⇧ adds to and ⌘ toggles against: the selection it started with by name (the
    /// cursor's item when nothing else was marked).
    pub base: HashSet<OsString>,
    /// The selection as it was, to put back on Esc: what was marked, the cursor and the anchor.
    pub before: (HashSet<OsString>, usize, usize),
    /// It started below the rows or in a gutter: a click there (no rectangle) clears the
    /// selection.
    pub on_empty: bool,
}

impl Band {
    /// The rectangle in content points: x, y, width, height.
    pub fn rect(&self) -> [f32; 4] {
        let (x0, x1) = (self.start[0].min(self.end[0]), self.start[0].max(self.end[0]));
        let (y0, y1) = (self.start[1].min(self.end[1]), self.start[1].max(self.end[1]));
        [x0, y0, x1 - x0, y1 - y0]
    }

    /// The pointer at window point `p` with the list scrolled to `scroll`, in content points.
    pub fn content(&self, p: [f32; 2], scroll: f32) -> [f32; 2] {
        [p[0] - self.view[0], p[1] - self.view[1] + scroll]
    }

    /// How far to scroll on a move to window point `p` (negative: up), when it's above or below
    /// the viewport: further the further out it is, at least 2 pt and at most two rows' worth.
    pub fn edge_step(&self, p: [f32; 2]) -> f32 {
        let (top, bottom) = (self.view[1], self.view[1] + self.view[3]);
        let out = if p[1] < top {
            p[1] - top
        } else if p[1] > bottom {
            p[1] - bottom
        } else {
            return 0.0;
        };
        (out * 0.5).abs().clamp(2.0, 48.0).copysign(out)
    }

    /// The selection it gives for what it touches now (`hits` of the folder's `visible` items).
    /// Returns false when that's what it already gave.
    pub fn apply(&mut self, f: &mut Folder) -> bool {
        let n = f.visible.len();
        let r = [self.start[0], self.start[1], self.end[0], self.end[1]];
        let hits = self.layout.hits(r, n);
        if hits == self.hits {
            return false;
        }
        self.hits = hits;
        let touched: Vec<usize> = self.layout.each(&self.hits, n).collect();
        let name = |i: usize| f.entries[f.visible[i] as usize].os.clone();
        let base = &self.base;
        let marked: HashSet<OsString> = match self.mode {
            Mode::Replace => touched.iter().map(|&i| name(i)).collect(),
            Mode::Add => base.iter().cloned().chain(touched.iter().map(|&i| name(i))).collect(),
            Mode::Toggle => {
                let mut m = base.clone();
                for &i in &touched {
                    let n = name(i);
                    if !m.remove(&n) {
                        m.insert(n);
                    }
                }
                m
            }
        };
        // The cursor follows the pointer's end of the rectangle, the anchor stays at its start,
        // so ⇧-arrows afterwards carry on from there.
        let down = self.end[1] >= self.start[1];
        let (near, far) = if down { (touched.first(), touched.last()) } else { (touched.last(), touched.first()) };
        if let (Some(&a), Some(&b)) = (near, far) {
            f.anchor = a;
            f.sel = b;
        }
        f.picks += 1;
        f.marked = marked;
        true
    }
}

impl crate::app::State {
    /// A press that may become a rectangle, in pane `id`'s list: the list's `on_drag` (empty
    /// space below the rows, a grid's gutters) and an unselected row's (the room after its
    /// name). `view` is the list's viewport (window points) and `layout` how its items lie, both
    /// as last laid out; `on_empty` says the press wasn't on a row.
    pub(crate) fn band_drag(&mut self, id: u64, d: &rune::Drag, layout: Layout, view: [f32; 4], on_empty: bool, tx: &mut rune::Tx) {
        tx.instant();
        let Some(ix) = self.pane_ix(id) else { return };
        match d.phase {
            rune::Phase::Down => {
                let f = &self.panes[ix].f;
                let scroll = f.scroll.get();
                let base = if f.marked.is_empty() { f.cursor_name().into_iter().collect() } else { f.marked.clone() };
                let mut b = Band {
                    pane: id,
                    version: f.version,
                    start: [0.0; 2],
                    end: [0.0; 2],
                    mode: Mode::of(tx.mods()),
                    layout,
                    view,
                    scroll,
                    moved: false,
                    hits: Hits { rows: usize::MAX..usize::MAX, cols: 0..0 },
                    base,
                    before: (f.marked.clone(), f.sel, f.anchor),
                    on_empty,
                };
                b.start = b.content(d.window_point(), scroll);
                b.end = b.start;
                self.band = Some(b);
                // A press that this one's row gets afterwards is a click only if it never moved.
                self.band_click = false;
            }
            rune::Phase::Move => {
                let Some(b) = self.band.as_mut().filter(|b| b.pane == id) else { return tx.live_only() };
                let f = &mut self.panes[ix].f;
                if f.version != b.version {
                    // The folder changed under it (a listing landed): it ends where it is.
                    self.band = None;
                    return;
                }
                if !b.moved {
                    if d.dx.hypot(d.dy) < START {
                        return tx.live_only();
                    }
                    b.moved = true;
                }
                let p = d.window_point();
                let mut scroll = f.scroll.get();
                let step = b.edge_step(p);
                if step != 0.0 {
                    let n = f.visible.len();
                    let content = match b.layout {
                        Layout::List { row_h } => n as f32 * row_h,
                        Layout::Grid(g) => g.height(n),
                    };
                    let to = (scroll + step).clamp(0.0, (content - b.view[3]).max(0.0));
                    if to != scroll {
                        tx.scroll_to_instant(f.list_key(), 0.0, to);
                        f.scroll.set(to);
                        scroll = to;
                    }
                }
                b.scroll = scroll;
                b.end = b.content(p, scroll);
                b.apply(f);
            }
            rune::Phase::Up => {
                let Some(b) = self.band.take().filter(|b| b.pane == id) else { return };
                let f = &mut self.panes[ix].f;
                if f.version != b.version {
                    return;
                }
                if d.cancelled() {
                    if b.moved {
                        (f.marked, f.sel, f.anchor) = b.before;
                    }
                    return;
                }
                self.band_click = b.moved;
                // A click in empty space clears a multiple selection (the cursor stays: a folder
                // always has one).
                if !b.moved && b.on_empty && b.mode == Mode::Replace && !f.marked.is_empty() {
                    f.picks += 1;
                    f.marked.clear();
                }
                self.sync_preview();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{INSET, IconSize, PAD};

    const LIST: Layout = Layout::List { row_h: 24.0 };

    #[test]
    fn list_hits_are_the_rows_touched() {
        assert_eq!(LIST.hits([10.0, 30.0, 500.0, 50.0], 100).rows, 1..3);
        assert_eq!(LIST.hits([500.0, 50.0, 10.0, 30.0], 100).rows, 1..3, "any corner order");
        assert_eq!(LIST.hits([0.0, 47.9, 0.0, 48.0], 100).rows, 1..3, "touching a row is enough");
        assert_eq!(LIST.hits([0.0, -40.0, 0.0, 5.0], 100).rows, 0..1, "above the top: from the first row");
        assert_eq!(LIST.hits([0.0, 2390.0, 0.0, 9999.0], 100).rows, 99..100, "below the rows: down to the last");
        assert!(LIST.hits([0.0, 2500.0, 0.0, 9999.0], 100).is_empty(), "only empty space");
        assert!(LIST.hits([0.0, -50.0, 0.0, -1.0], 100).is_empty());
        assert!(LIST.hits([0.0, 0.0, 0.0, 100.0], 0).is_empty());
        let h = LIST.hits([0.0, 30.0, 0.0, 80.0], 100);
        assert_eq!(LIST.each(&h, 100).collect::<Vec<_>>(), [1, 2, 3]);
    }

    #[test]
    fn grid_hits_are_the_cells_touched_not_the_gutters() {
        // Medium: 116 × 120 cells, 4 columns, 10 items (rows of 4, 4, 2).
        let g = Geom { cols: 4, cell: IconSize::Medium.cell() };
        let grid = Layout::Grid(g);
        let at = |c: f32, r: f32| [PAD + c * 116.0, r * 120.0];
        // Inside the gutter between columns 0 and 1 and rows 0 and 1: nothing.
        let [gx, gy] = at(1.0, 1.0);
        assert!(grid.hits([gx - 2.0, gy - 2.0, gx + 2.0, gy + 2.0], 10).is_empty());
        // From the middle of item 1 to the middle of item 6: columns 1–2, rows 0–1.
        let (a, b) = (at(1.5, 0.5), at(2.5, 1.5));
        let h = grid.hits([a[0], a[1], b[0], b[1]], 10);
        assert_eq!((h.rows.clone(), h.cols.clone()), (0..2, 1..3));
        assert_eq!(grid.each(&h, 10).collect::<Vec<_>>(), [1, 2, 5, 6]);
        // Reaching just inside an item's edge counts; stopping in the gutter before it doesn't.
        let edge = PAD + 2.0 * 116.0 + INSET;
        assert_eq!(grid.hits([a[0], a[1], edge + 0.5, a[1]], 10).cols, 1..3);
        assert_eq!(grid.hits([a[0], a[1], edge - 0.5, a[1]], 10).cols, 1..2);
        // The short last row: cells past its end are no items.
        let h = grid.hits([0.0, 0.0, 9999.0, 9999.0], 10);
        assert_eq!(grid.each(&h, 10).count(), 10);
        // Right of the last column only: nothing.
        assert!(grid.hits([PAD + 4.0 * 116.0 + 1.0, 0.0, 9999.0, 9999.0], 10).is_empty());
    }

    /// Scrolled: the same geometry in content points, so the window point plus the offset.
    #[test]
    fn hits_follow_the_scroll_offset() {
        let b = band(LIST, Mode::Replace, &[]);
        assert_eq!(b.content([110.0, 70.0], 0.0), [10.0, 20.0]);
        assert_eq!(b.content([110.0, 70.0], 480.0), [10.0, 500.0]);
        let r = b.content([110.0, 70.0], 480.0);
        assert_eq!(LIST.hits([0.0, r[1], 0.0, r[1]], 100).rows, 20..21);
        // Past the edges it scrolls, more the further out; inside, not at all.
        assert_eq!(b.edge_step([0.0, 300.0]), 0.0);
        assert_eq!(b.edge_step([0.0, 49.0]), -2.0);
        assert_eq!(b.edge_step([0.0, 50.0 + 400.0 + 40.0]), 20.0);
        assert_eq!(b.edge_step([0.0, 5000.0]), 48.0);
    }

    fn band(layout: Layout, mode: Mode, before: &[&str]) -> Band {
        Band {
            pane: 0,
            version: 0,
            start: [0.0; 2],
            end: [0.0; 2],
            mode,
            layout,
            view: [100.0, 50.0, 600.0, 400.0],
            scroll: 0.0,
            moved: true,
            hits: Hits::default(),
            base: before.iter().map(OsString::from).collect(),
            before: (HashSet::new(), 0, 0),
            on_empty: false,
        }
    }

    fn folder(n: usize) -> Folder {
        let mut f = Folder::new(9, std::env::temp_dir(), crate::fs::Sort { key: crate::fs::SortKey::Name, desc: false }, false, None);
        f.entries = (0..n).map(|i| crate::fs::Entry::test(&format!("f{i:02}"), false)).collect();
        f.refilter(None);
        f
    }

    fn selected(f: &Folder) -> Vec<usize> {
        (0..f.visible.len()).filter(|&i| f.is_selected(i)).collect()
    }

    #[test]
    fn modifiers_replace_add_or_toggle() {
        let mut f = folder(10);
        // Rows 2..=4.
        let drag = |f: &mut Folder, mode, before: &[&str]| {
            let mut b = band(LIST, mode, before);
            b.start = [5.0, 2.0 * 24.0 + 3.0];
            b.end = [5.0, 4.0 * 24.0 + 3.0];
            assert!(b.apply(f));
            assert!(!b.apply(f), "nothing new");
            b
        };
        drag(&mut f, Mode::Replace, &["f00", "f07"]);
        assert_eq!(selected(&f), [2, 3, 4]);
        assert_eq!((f.anchor, f.sel), (2, 4), "the cursor at the pointer's end");
        drag(&mut f, Mode::Add, &["f00", "f07"]);
        assert_eq!(selected(&f), [0, 2, 3, 4, 7]);
        drag(&mut f, Mode::Toggle, &["f00", "f03"]);
        assert_eq!(selected(&f), [0, 2, 4], "f03 was selected: toggled off");
        // Dragged back up past where it started: the rows between, the cursor at the top.
        let mut b = band(LIST, Mode::Replace, &[]);
        b.start = [5.0, 5.0 * 24.0 + 1.0];
        b.end = [5.0, 3.0 * 24.0 + 1.0];
        b.apply(&mut f);
        assert_eq!(selected(&f), [3, 4, 5]);
        assert_eq!((f.anchor, f.sel), (5, 3));
        // Shrunk back to touching nothing but empty space below: nothing marked.
        b.start = [5.0, 900.0];
        b.end = [5.0, 950.0];
        assert!(b.apply(&mut f));
        assert!(f.marked.is_empty());
    }

    #[test]
    fn grid_selection_from_geometry() {
        let mut f = folder(10);
        let g = Geom { cols: 4, cell: IconSize::Medium.cell() };
        let mut b = band(Layout::Grid(g), Mode::Replace, &[]);
        b.start = [PAD + 1.5 * 116.0, 0.5 * 120.0];
        b.end = [PAD + 2.5 * 116.0, 2.5 * 120.0];
        b.apply(&mut f);
        assert_eq!(selected(&f), [1, 2, 5, 6, 9], "item 10 isn't there");
    }

    #[test]
    fn mode_from_modifiers() {
        let m = |cmd, shift| Mode::of(rune::Mods { cmd, shift, ..rune::Mods::default() });
        assert_eq!(m(false, false), Mode::Replace);
        assert_eq!(m(false, true), Mode::Add);
        assert_eq!(m(true, false), Mode::Toggle);
        assert_eq!(m(true, true), Mode::Toggle, "⌘ wins, as with clicks");
    }
}
