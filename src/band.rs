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
//! (RUNE-GAPS.md). The wheel scrolling the list under a held rectangle moves it with the
//! content too (the rows' layout report on each scrolled frame), selecting what scrolls in.

use std::collections::HashMap;
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
    #[cfg(test)]
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
#[cfg_attr(test, derive(Clone))]
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
    /// The pointer in window points, last seen: where the far corner stays when the list
    /// scrolls under a still pointer (the wheel).
    pub pointer: [f32; 2],
    /// It has travelled far enough to be a rectangle.
    pub moved: bool,
    /// What it touched when last applied.
    pub hits: Hits,
    /// It has applied its first selection (`apply`).
    pub started: bool,
    /// The rows it touches now, each with whether it was selected before the rectangle reached
    /// it: what a row that leaves the rectangle goes back to, and what ⌘ flips. Only what it
    /// touches: a move costs what it changes, never the whole selection. (A listing landing
    /// ends the rectangle, so rows stay put while it's dragged.)
    pub orig: HashMap<usize, bool>,
    /// Alone (no ⇧ or ⌘): the selection it started with (and its totals), moved aside (not
    /// copied), for Esc.
    pub cleared: Option<crate::marked::Marked>,
    /// ⇧ or ⌘ with only the cursor selected: the cursor's row, put in the selection first (it
    /// was selected, as the cursor), and taken out again on Esc.
    pub seeded: Option<usize>,
    /// The cursor and the anchor as they were, for Esc.
    pub before: (usize, usize),
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

    /// The selection it gives for what it touches now (`hits` of the folder's `visible` items),
    /// changing only the items that entered or left the rectangle since the last time. Returns
    /// false when nothing changed.
    pub fn apply(&mut self, f: &mut Folder) -> bool {
        let n = f.visible.len();
        let r = [self.start[0], self.start[1], self.end[0], self.end[1]];
        let hits = self.layout.hits(r, n);
        if self.started && hits == self.hits {
            return false;
        }
        if !std::mem::replace(&mut self.started, true) {
            match self.mode {
                Mode::Replace => self.cleared = Some(std::mem::take(&mut f.marked)),
                // The cursor alone counts as selected: ⇧ adds to it, ⌘ toggles against it.
                _ if f.marked.is_empty() => {
                    let c = f.sel;
                    f.marked.set(c, true, &f.visible);
                    self.seeded = Some(c);
                }
                _ => {}
            }
        }
        let old = std::mem::replace(&mut self.hits, hits);
        let cols = self.layout.cols();
        let (mut left, mut entered) = (Vec::new(), Vec::new());
        minus(cols, &old, &self.hits, n, &mut left);
        minus(cols, &self.hits, &old, n, &mut entered);
        for i in left {
            if let Some(was) = self.orig.remove(&i) {
                f.marked.set(i, was, &f.visible);
            }
        }
        for i in entered {
            let was = f.marked.contains(i);
            self.orig.insert(i, was);
            let on = self.mode != Mode::Toggle || !was;
            f.marked.set(i, on, &f.visible);
        }
        // The cursor follows the pointer's end of the rectangle, the anchor stays at its start,
        // so ⇧-arrows afterwards carry on from there.
        if let Some((first, last)) = ends(cols, &self.hits, n) {
            let down = self.end[1] >= self.start[1];
            (f.anchor, f.sel) = if down { (first, last) } else { (last, first) };
        }
        f.picks += 1;
        true
    }

    /// Esc: the selection, cursor and anchor as they were before it.
    pub fn cancel(&mut self, f: &mut Folder) {
        if let Some(m) = self.cleared.take() {
            f.marked = m;
        } else {
            for (row, was) in self.orig.drain() {
                f.marked.set(row, was, &f.visible);
            }
            if let Some(c) = self.seeded.take() {
                f.marked.set(c, false, &f.visible);
            }
        }
        (f.sel, f.anchor) = self.before;
    }
}

impl Layout {
    /// Items per row.
    fn cols(self) -> usize {
        match self {
            Layout::List { .. } => 1,
            Layout::Grid(g) => g.cols.max(1),
        }
    }
}

/// `x` less `y`, as at most two ranges.
fn less(x: &Range<usize>, y: &Range<usize>) -> [Range<usize>; 2] {
    [x.start..x.end.min(y.start).max(x.start), x.start.max(y.end).min(x.end)..x.end]
}

/// Pushes the items (below `n`) in `a` and not in `b`, a grid of `cols` columns: whole rows of
/// `a` outside `b`'s rows, and in the rows both share, `a`'s columns outside `b`'s. Work in
/// proportion to what differs, however large `a` and `b` are.
fn minus(cols: usize, a: &Hits, b: &Hits, n: usize, out: &mut Vec<usize>) {
    let mut push = |r: usize, cs: &Range<usize>| {
        for c in cs.clone() {
            let i = r * cols + c;
            if i < n {
                out.push(i);
            }
        }
    };
    if a.is_empty() {
        return;
    }
    if b.is_empty() {
        for r in a.rows.clone() {
            push(r, &a.cols);
        }
        return;
    }
    for rows in less(&a.rows, &b.rows) {
        for r in rows {
            push(r, &a.cols);
        }
    }
    let side = less(&a.cols, &b.cols);
    if side.iter().any(|c| !c.is_empty()) {
        for r in a.rows.start.max(b.rows.start)..a.rows.end.min(b.rows.end) {
            for cs in &side {
                push(r, cs);
            }
        }
    }
}

/// The first and last items (below `n`) of `h`, in display order.
fn ends(cols: usize, h: &Hits, n: usize) -> Option<(usize, usize)> {
    if h.is_empty() {
        return None;
    }
    let first = h.rows.start * cols + h.cols.start;
    if first >= n {
        return None;
    }
    let r = h.rows.end - 1;
    let mut last = r * cols + h.cols.end - 1;
    if last >= n {
        // The last row is short: its last item, if it reaches the columns, else the row above's.
        last = if n > r * cols + h.cols.start { n - 1 } else { (r - 1) * cols + h.cols.end - 1 };
    }
    Some((first, last))
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
                let mut b = Band {
                    pane: id,
                    version: f.version,
                    start: [0.0; 2],
                    end: [0.0; 2],
                    mode: Mode::of(tx.mods()),
                    layout,
                    view,
                    scroll,
                    pointer: d.window_point(),
                    moved: false,
                    hits: Hits::default(),
                    started: false,
                    orig: HashMap::new(),
                    cleared: None,
                    seeded: None,
                    before: (f.sel, f.anchor),
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
                b.pointer = p;
                b.end = b.content(p, scroll);
                b.apply(f);
            }
            rune::Phase::Up => {
                let Some(mut b) = self.band.take().filter(|b| b.pane == id) else { return };
                let f = &mut self.panes[ix].f;
                if f.version != b.version {
                    return;
                }
                if d.cancelled() {
                    if b.started {
                        b.cancel(f);
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

impl crate::app::State {
    /// Pane `id`'s rows are laid out at window y `top` (their element's layout report, which
    /// comes on every frame a scroll moves them): a rectangle being dragged out follows the
    /// scroll (the wheel under a still pointer), its far corner staying under the pointer in
    /// the content, and selects what scrolled into it. False when there's nothing to follow,
    /// so the report changes nothing (`Tx::live_only`).
    pub(crate) fn band_scrolled(&mut self, id: u64, top: f32) -> bool {
        let Some(b) = self.band.as_mut().filter(|b| b.pane == id && b.moved) else { return false };
        let Some(ix) = self.panes.iter().position(|p| p.id() == id) else { return false };
        let f = &mut self.panes[ix].f;
        let scroll = (b.view[1] - top).max(0.0);
        if (scroll - b.scroll).abs() < 0.5 || f.version != b.version {
            return false;
        }
        b.scroll = scroll;
        b.end = b.content(b.pointer, scroll);
        b.apply(f);
        true
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

    fn band(layout: Layout, mode: Mode, _before: &[&str]) -> Band {
        Band {
            pane: 0,
            version: 0,
            start: [0.0; 2],
            end: [0.0; 2],
            mode,
            layout,
            view: [100.0, 50.0, 600.0, 400.0],
            scroll: 0.0,
            pointer: [0.0; 2],
            moved: true,
            hits: Hits::default(),
            started: false,
            orig: HashMap::new(),
            cleared: None,
            seeded: None,
            before: (0, 0),
            on_empty: false,
        }
    }

    fn folder(n: usize) -> Folder {
        let mut f = Folder::new(9, std::env::temp_dir(), crate::fs::Sort::new(crate::fs::SortKey::Name, false), false, None);
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
            f.mark_names(before);
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

    /// Rows a rectangle leaves go back to what they were: with ⇧ to the selection it started
    /// with, with ⌘ unflipped; and Esc puts everything back, the cursor's item too when it was
    /// all that was selected.
    #[test]
    fn rows_it_leaves_go_back_and_esc_puts_it_all_back() {
        let mut f = folder(10);
        let names = |f: &Folder| selected(f);
        f.mark_names(&["f01", "f03"]);
        let mut b = band(LIST, Mode::Toggle, &[]);
        b.start = [5.0, 1.0];
        b.end = [5.0, 4.0 * 24.0 + 1.0];
        b.apply(&mut f);
        assert_eq!(names(&f), [0, 2, 4], "rows 0–4 flipped");
        b.end = [5.0, 2.0 * 24.0 + 1.0];
        b.apply(&mut f);
        assert_eq!(names(&f), [0, 2, 3], "rows 3 and 4 back as they were");
        b.cancel(&mut f);
        assert_eq!(names(&f), [1, 3]);
        // ⇧ with only the cursor selected: it counts, and Esc leaves just the cursor.
        f.marked.clear();
        f.select(6);
        let mut b = band(LIST, Mode::Add, &[]);
        b.before = (6, 6);
        b.start = [5.0, 1.0];
        b.end = [5.0, 1.0 * 24.0 + 1.0];
        b.apply(&mut f);
        assert_eq!(names(&f), [0, 1, 6]);
        b.cancel(&mut f);
        assert!(f.marked.is_empty() && f.sel == 6);
        // Alone: the old selection is set aside and comes back whole on Esc.
        f.mark_names(&["f08", "f09"]);
        let mut b = band(LIST, Mode::Replace, &[]);
        b.start = [5.0, 1.0];
        b.end = [5.0, 1.0];
        b.apply(&mut f);
        assert_eq!(names(&f), [0]);
        b.cancel(&mut f);
        assert_eq!(names(&f), [8, 9]);
    }

    /// A folder of `n` files, file i `(i + 1) × 100` bytes, folder rows every tenth.
    fn sized(n: usize) -> Folder {
        let mut f = folder(n);
        for (i, e) in f.entries.iter_mut().enumerate() {
            e.size = (i as u64 + 1) * 100;
            e.is_dir = i % 10 == 9;
        }
        f.refilter(None);
        f
    }

    /// The selection's total moves by what a change changes: a ⌘-click, a rectangle growing
    /// and shrinking, never a look through the listing (the rows' running sums say each row's
    /// size). Every entry's size is changed behind the selection's back: a total worked out
    /// again from the listing would show it.
    #[test]
    fn a_selection_change_costs_what_it_changes() {
        let mut f = sized(1000);
        f.select(0);
        f.toggle_mark(1);
        let p = f.picked_size().unwrap();
        assert_eq!(p, crate::marked::Picked { count: 2, bytes: 300, folders: 0 });
        for e in &mut f.entries {
            e.size *= 1000;
        }
        // Row 5: 600 bytes when the rows were made (600,000 now, unseen).
        f.toggle_mark(5);
        assert_eq!(f.picked_size().unwrap(), crate::marked::Picked { count: 3, bytes: 900, folders: 0 });
        // A rectangle over rows 10..=19 (row 19 a folder), then back to 10..=14.
        let mut b = band(LIST, Mode::Add, &[]);
        b.start = [5.0, 10.0 * 24.0 + 1.0];
        b.end = [5.0, 19.0 * 24.0 + 1.0];
        b.apply(&mut f);
        let rows: u64 = (11..=19).map(|i| i * 100).sum();
        assert_eq!(f.picked_size().unwrap(), crate::marked::Picked { count: 13, bytes: 900 + rows, folders: 1 });
        b.end = [5.0, 14.0 * 24.0 + 1.0];
        b.apply(&mut f);
        let rows: u64 = (11..=15).map(|i| i * 100).sum();
        assert_eq!(f.picked_size().unwrap(), crate::marked::Picked { count: 8, bytes: 900 + rows, folders: 0 });
    }

    /// Every selection change at 500,000 rows costs what it changes, not the folder: a click,
    /// ⌘-click, ⇧ growing and shrinking a range by a row, ⌘A, Invert, Esc, a rectangle's move
    /// and its Esc. None of them looks at the listing (its entries are taken away once the rows
    /// are made: a look would panic), and in a release build each takes under 1 ms (the best
    /// of five: a busy machine can hold up any one). They took up to 238 ms.
    #[test]
    fn every_selection_change_is_quick_at_500k() {
        let n = 500_000;
        let mut f = folder(0);
        f.entries =
            (0..n).map(|i| crate::fs::Entry { size: i as u64, ..crate::fs::Entry::test(&format!("f{i:06}"), i % 50 == 0) }).collect();
        f.refilter(None);
        let entries = std::mem::take(&mut f.entries);
        let mut worst = Vec::new();
        let mut time = |what: &str, f: &mut Folder, op: &dyn Fn(&mut Folder)| {
            let best = (0..5)
                .map(|_| {
                    let mut g = folder(0);
                    (g.marked, g.visible, g.sel, g.anchor) = (f.marked.clone(), f.visible.clone(), f.sel, f.anchor);
                    let t = std::time::Instant::now();
                    op(&mut g);
                    let e = t.elapsed();
                    std::hint::black_box(g.picked_size());
                    e
                })
                .min()
                .unwrap();
            op(f);
            worst.push((what.to_string(), best));
        };
        time("⌘A", &mut f, &|f| f.select_all());
        time("a click (clears 500k)", &mut f, &|f| f.select(250_000));
        time("⇧ to the end", &mut f, &|f| f.extend_to(n - 1));
        time("⇧ shrinks a row", &mut f, &|f| f.extend_to(n - 2));
        time("⇧ grows a row", &mut f, &|f| f.extend_to(n - 1));
        time("⌘-click", &mut f, &|f| f.toggle_mark(10));
        time("⇧ after a ⌘-click", &mut f, &|f| f.extend_to(0));
        time("Invert", &mut f, &|f| {
            f.invert_selection();
        });
        time("Esc (clears)", &mut f, &|f| f.marked.clear());
        // A rectangle over 400k rows (made once, as it would be over many moves), then a row
        // more and a row less at its edge, each from the same start.
        f.marked.clear();
        let mut b = band(LIST, Mode::Replace, &[]);
        (b.start, b.end) = ([5.0, 1.0], [5.0, 400_000.0 * 24.0 - 10.0]);
        b.apply(&mut f);
        for (what, to) in [("the rectangle grows a row", 400_001.0), ("the rectangle shrinks a row", 399_999.0)] {
            let best = (0..5)
                .map(|_| {
                    let (mut g, mut c) = (folder(0), b.clone());
                    (g.marked, g.visible, g.sel, g.anchor) = (f.marked.clone(), f.visible.clone(), f.sel, f.anchor);
                    c.end = [5.0, to * 24.0 - 10.0];
                    let t = std::time::Instant::now();
                    c.apply(&mut g);
                    let e = t.elapsed();
                    assert_eq!(g.picked_size().map(|p| p.count), Some(to as usize));
                    e
                })
                .min()
                .unwrap();
            worst.push((what.to_string(), best));
        }
        assert_eq!(f.picked_size().map(|p| p.count), Some(400_000), "the rectangle's rows");
        b.cancel(&mut f);
        assert!(f.marked.is_empty());
        drop(entries);
        for (what, t) in &worst {
            eprintln!("{what}: {:.3} ms", t.as_secs_f64() * 1e3);
            if !cfg!(debug_assertions) {
                assert!(*t < std::time::Duration::from_millis(1), "{what}: {t:?} at 500k rows");
            }
        }
    }

    /// What's selected, what the status bar adds up and what an action acts on are the same
    /// items: hiding one (hidden files turned off) takes it out of all three.
    #[test]
    fn hidden_items_leave_the_selection_and_its_total() {
        let mut f = sized(10);
        f.entries[3].hidden = true;
        f.set_hidden(true);
        f.select(2);
        f.toggle_mark(3);
        f.toggle_mark(4);
        assert_eq!(f.picked_size().map(|p| p.count), Some(3));
        f.set_hidden(false);
        let targets: Vec<(String, u64)> = f.targets().iter().map(|e| (e.name.clone(), e.size)).collect();
        assert_eq!(targets, [("f02".to_string(), 300), ("f04".to_string(), 500)]);
        let p = f.picked_size().unwrap();
        assert_eq!((p.count, p.bytes), (targets.len(), targets.iter().map(|t| t.1).sum::<u64>()), "the total is the targets'");
        // Shown again, it stays out (it was deselected, not set aside).
        f.set_hidden(true);
        assert_eq!(f.picked_size().map(|p| p.count), Some(2));
    }

    /// Esc on a rectangle puts the selection's total back with the selection, whichever way it
    /// started (alone, setting the old selection aside; ⇧ or ⌘, changing items one by one),
    /// though as many items are selected before and during it.
    #[test]
    fn a_cancelled_rectangle_puts_the_total_back() {
        for mode in [Mode::Replace, Mode::Add, Mode::Toggle] {
            let mut f = sized(100);
            f.select(0);
            f.toggle_mark(1);
            let before = f.picked_size().unwrap();
            assert_eq!(before.bytes, 300);
            // Over rows 4 and 5 (alone: they replace rows 0 and 1, two for two).
            let mut b = band(LIST, mode, &[]);
            b.start = [5.0, 4.0 * 24.0 + 1.0];
            b.end = [5.0, 5.0 * 24.0 + 1.0];
            b.apply(&mut f);
            assert_ne!(f.picked_size(), Some(before), "{mode:?}: it changed the selection");
            b.cancel(&mut f);
            assert_eq!(f.picked_size(), Some(before), "{mode:?}");
        }
    }

    /// The items between two rectangles, in a grid and a list, are exactly those that differ.
    #[test]
    fn the_difference_is_what_entered_or_left() {
        let h = |rows: Range<usize>, cols: Range<usize>| Hits { rows, cols };
        let mut out = Vec::new();
        minus(4, &h(0..3, 1..3), &h(1..3, 2..4), 10, &mut out);
        out.sort_unstable();
        assert_eq!(out, [1, 2, 5, 9], "row 0 whole, then column 1 of rows 1 and 2 (10 isn't there)");
        out.clear();
        minus(1, &h(0..1_000_000, 0..1), &h(1..1_000_000, 0..1), 2_000_000, &mut out);
        assert_eq!(out, [0], "a list: one row, not a million looked at");
        assert_eq!(ends(4, &h(1..3, 1..4), 10), Some((5, 9)), "the short last row's last item");
        assert_eq!(ends(4, &h(1..3, 2..4), 9), Some((6, 7)), "past the short row: the row above");
    }

    /// A move costs what it changes, not what's selected: with 100,000 and 500,000 items
    /// already selected, ⇧ and ⌘ rectangles growing a row at a time take well under a
    /// millisecond a move (each used to clone the whole selection: 24 and 227 ms in release).
    #[test]
    fn a_move_costs_what_it_changes_not_whats_selected() {
        for n in [100_000, 500_000] {
            let mut f = folder(n);
            f.select_all();
            for mode in [Mode::Add, Mode::Toggle] {
                let mut b = band(LIST, mode, &[]);
                b.start = [10.0, 5.0];
                let mut times = Vec::new();
                for i in 0..20 {
                    b.end = [10.0, 100.0 + i as f32 * 24.0];
                    let at = std::time::Instant::now();
                    b.apply(&mut f);
                    times.push(at.elapsed().as_secs_f64() * 1000.0);
                }
                times.sort_by(f64::total_cmp);
                let (median, worst) = (times[times.len() / 2], times[times.len() - 1]);
                println!("{n} selected, {mode:?}: median {median:.4} ms, worst {worst:.4} ms");
                // The median: one move the scheduler preempted on a busy machine (a parallel
                // test run) took over a millisecond and failed the worst-case bound, while
                // cloning the selection costs every move tens of milliseconds.
                assert!(median < 1.0, "{n} selected, {mode:?}: median {median:.3} ms (worst {worst:.3})");
                b.cancel(&mut f);
                assert_eq!(f.marked.len(), n, "Esc puts it back");
            }
        }
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
