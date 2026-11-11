//! The grid views' geometry: a tab's view mode, the icon sizes, and where each item's cell is.
//! The icon view and the tiles view are both grids (the tiles' cells are wider, with the
//! icon beside the details instead of over the name).
//!
//! The grid is fixed-size cells laid out left to right, top to bottom, as many to a row as the
//! pane's width takes. Everything here is arithmetic on that layout, so keyboard movement,
//! drag-select hits and the thumbnail scheduler all agree with what's drawn without asking which
//! rows are built (rows are virtual: most aren't).

/// How a tab shows its folder. Remembered per tab (`TabMemo::view`); new tabs take
/// `view.default` from the settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ViewMode {
    /// Rows with columns (name, kind, size, date): the view Arbor always had.
    #[default]
    List,
    /// A grid of icons (and thumbnails) with names under them.
    Icons,
    /// Finder's column browser: a column per folder from a root down to this one, the
    /// selected folder's contents (or the selected file's preview) to the right (`columns.rs`).
    Columns,
    /// A grid of wide tiles: a larger icon (or thumbnail) beside the name, kind and size.
    Tiles,
}

impl ViewMode {
    /// Reads `view.default` from the settings.
    pub fn named(s: &str) -> Option<ViewMode> {
        match s {
            "list" => Some(ViewMode::List),
            "icons" | "grid" => Some(ViewMode::Icons),
            "columns" => Some(ViewMode::Columns),
            "tiles" => Some(ViewMode::Tiles),
            _ => None,
        }
    }

    /// Laid out as a grid of cells (the icon view's machinery: 2-D arrows, cell hits,
    /// thumbnails for the rows on screen).
    pub fn is_grid(self) -> bool {
        matches!(self, ViewMode::Icons | ViewMode::Tiles)
    }

    /// The grid this view lays out in a list `width` points wide (`None`: not a grid).
    pub fn geom(self, size: IconSize, width: f32) -> Option<Geom> {
        match self {
            ViewMode::Icons => Some(Geom::new(size, width)),
            ViewMode::Tiles => Some(Geom::tiles(width)),
            ViewMode::List | ViewMode::Columns => None,
        }
    }

    /// The side of a cell's icon (and its thumbnail), in points.
    pub fn icon(self, size: IconSize) -> f32 {
        match self {
            ViewMode::Tiles => TILE_ICON,
            _ => size.icon(),
        }
    }
}

/// A tile's icon side, its cell's size and the narrowest it gets, in points. Tiles have one
/// size (⌘+ and ⌘- are the icon view's): wide enough for a name, its kind and its size beside
/// a 48 pt icon, and they widen to fill the row (as many columns as fit at the narrowest).
pub const TILE_ICON: f32 = 48.0;
pub const TILE_W: f32 = 248.0;
pub const TILE_H: f32 = 72.0;

/// The icon view's size. One for the window (⌘+ and ⌘- change it); `view.icon_size` in the
/// settings is where it starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum IconSize {
    Small,
    #[default]
    Medium,
    Large,
}

impl IconSize {
    pub fn named(s: &str) -> Option<IconSize> {
        match s {
            "small" => Some(IconSize::Small),
            "medium" => Some(IconSize::Medium),
            "large" => Some(IconSize::Large),
            _ => None,
        }
    }

    /// The next size up (`by` 1) or down (`by` -1), if there is one.
    pub fn step(self, by: i32) -> Option<IconSize> {
        use IconSize::*;
        match (self, by.signum()) {
            (Small, 1) => Some(Medium),
            (Medium, 1) => Some(Large),
            (Large, -1) => Some(Medium),
            (Medium, -1) => Some(Small),
            _ => None,
        }
    }

    /// The icon's side, in points.
    pub fn icon(self) -> f32 {
        match self {
            IconSize::Small => 32.0,
            IconSize::Medium => 64.0,
            IconSize::Large => 128.0,
        }
    }

    /// A cell's width and height, in points: the icon, room for two lines of name under it,
    /// and the gutter around the item.
    pub fn cell(self) -> [f32; 2] {
        match self {
            IconSize::Small => [88.0, 88.0],
            IconSize::Medium => [116.0, 120.0],
            IconSize::Large => [172.0, 184.0],
        }
    }
}

/// Space between the list's left edge and the first column, in points. (Rows start at the top:
/// each cell's inset is the room above the first.)
pub const PAD: f32 = 8.0;
/// Space between a cell's edge and its item, on each side: the gutters between items are twice
/// this, and a press there starts a drag-select rather than acting on an item.
pub const INSET: f32 = 6.0;

/// One grid's layout: the cell size and how many columns fit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geom {
    pub cols: usize,
    pub cell: [f32; 2],
}

impl Geom {
    /// The grid for `size` in a list `width` points wide (at least one column).
    pub fn new(size: IconSize, width: f32) -> Geom {
        let cell = size.cell();
        let cols = ((width - 2.0 * PAD) / cell[0]).floor();
        Geom { cols: if cols.is_finite() && cols >= 1.0 { cols as usize } else { 1 }, cell }
    }

    /// The tiles view's grid in a list `width` points wide: as many [`TILE_W`] columns as fit,
    /// each widened to share what's left over (so the tiles line up with both edges).
    pub fn tiles(width: f32) -> Geom {
        let room = width - 2.0 * PAD;
        let cols = (room / TILE_W).floor();
        let cols = if cols.is_finite() && cols >= 1.0 { cols as usize } else { 1 };
        // An absurd width (before the first layout, `f32::MAX`) keeps the plain tile.
        let w = if room > TILE_W && room < 100_000.0 { (room / cols as f32).floor() } else { TILE_W };
        Geom { cols, cell: [w, TILE_H] }
    }

    /// Rows for `n` items.
    pub fn rows(self, n: usize) -> usize {
        n.div_ceil(self.cols.max(1))
    }

    /// The content height of `n` items, in points.
    pub fn height(self, n: usize) -> f32 {
        self.rows(n) as f32 * self.cell[1]
    }

    #[cfg(test)]
    /// Item `i`'s rectangle (x, y, w, h) in content points: its cell less the inset.
    pub fn item_rect(self, i: usize) -> [f32; 4] {
        let (r, c) = (i / self.cols.max(1), i % self.cols.max(1));
        let [w, h] = self.cell;
        [PAD + c as f32 * w + INSET, r as f32 * h + INSET, w - 2.0 * INSET, h - 2.0 * INSET]
    }

    #[cfg(test)]
    /// The first and one-past-last rows a viewport from `top` to `top + h` (content points)
    /// shows any part of, clamped to `n` items' rows.
    pub fn rows_in(self, top: f32, h: f32, n: usize) -> std::ops::Range<usize> {
        let rows = self.rows(n);
        let a = (top / self.cell[1]).floor().max(0.0) as usize;
        let b = ((top + h) / self.cell[1]).ceil().max(0.0) as usize;
        a.min(rows)..b.min(rows)
    }
}

/// Where the cursor goes from `i` on an arrow in a grid of `cols` columns and `n` items: left
/// and right by one (wrapping into the row before or after), up and down by a row. Down from
/// the last full row into a shorter one lands on its last item, as Finder's does; up from the
/// first row and down from the last stay put.
pub fn step(i: usize, cols: usize, n: usize, dx: isize, dy: isize) -> usize {
    if n == 0 {
        return 0;
    }
    let cols = cols.max(1) as isize;
    let (i, last) = (i.min(n - 1) as isize, n as isize - 1);
    if dx != 0 {
        return (i + dx).clamp(0, last) as usize;
    }
    let to = i + dy * cols;
    if to < 0 {
        // Up from the first row: by whole rows only, as far as there are rows.
        let rows = i / cols;
        return (i - rows.min(dy.abs()) * cols) as usize;
    }
    if to > last {
        // Down past the end: into the last row if it's below this one, else stay.
        let (row, last_row) = (i / cols, last / cols);
        return if last_row > row { (last_row * cols + i % cols).min(last) as usize } else { i as usize };
    }
    to as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_fit_the_width() {
        let g = Geom::new(IconSize::Medium, 2.0 * PAD + 116.0 * 5.0 + 115.0);
        assert_eq!(g.cols, 5);
        assert_eq!(Geom::new(IconSize::Large, 40.0).cols, 1, "always one column");
        assert!(Geom::new(IconSize::Small, f32::MAX).cols > 1);
        assert_eq!(g.rows(11), 3);
        assert_eq!(g.rows(0), 0);
        assert_eq!(g.item_rect(6), [PAD + 116.0 + INSET, 120.0 + INSET, 116.0 - 2.0 * INSET, 120.0 - 2.0 * INSET]);
        // A viewport from 130 to 250 shows rows 1 and 2 (row 1 spans 120..240).
        assert_eq!(g.rows_in(130.0, 120.0, 100), 1..3);
        assert_eq!(g.rows_in(120.0, 120.0, 100), 1..2, "exactly one row");
        assert_eq!(g.rows_in(0.0, 10_000.0, 11), 0..3, "clamped to the rows there are");
    }

    #[test]
    fn arrows_move_by_cell_and_row() {
        // 4 columns, 10 items: rows [0..4), [4..8), [8, 9].
        let s = |i, dx, dy| step(i, 4, 10, dx, dy);
        assert_eq!(s(5, 1, 0), 6);
        assert_eq!(s(4, -1, 0), 3, "left from a row's start goes to the row before's end");
        assert_eq!(s(9, 1, 0), 9, "the last item stays");
        assert_eq!(s(0, -1, 0), 0);
        assert_eq!(s(1, 0, 1), 5);
        assert_eq!(s(5, 0, -1), 1);
        assert_eq!(s(2, 0, -1), 2, "up from the first row stays");
        assert_eq!(s(6, 0, 1), 9, "down into a shorter last row: its last item");
        assert_eq!(s(9, 0, 1), 9, "down from the last row stays");
        assert_eq!(s(1, 0, 20), 9, "a page down past the end: the last row");
        assert_eq!(s(9, 0, -20), 1, "a page up past the start: the first row, same column");
        assert_eq!(step(3, 4, 0, 1, 0), 0);
    }

    #[test]
    fn sizes_step_and_parse() {
        assert_eq!(IconSize::Small.step(1), Some(IconSize::Medium));
        assert_eq!(IconSize::Large.step(1), None);
        assert_eq!(IconSize::Small.step(-1), None);
        assert_eq!(IconSize::named("large"), Some(IconSize::Large));
        assert_eq!(ViewMode::named("icons"), Some(ViewMode::Icons));
        assert_eq!(ViewMode::named("columns"), Some(ViewMode::Columns));
        assert_eq!(ViewMode::named("tiles"), Some(ViewMode::Tiles));
        assert_eq!(ViewMode::named("gallery"), None);
        assert!(ViewMode::Tiles.is_grid() && ViewMode::Icons.is_grid());
        assert!(!ViewMode::Columns.is_grid() && !ViewMode::List.is_grid());
    }

    #[test]
    fn tiles_fill_the_row() {
        // 2 × 248 fits in 600 less the padding; each tile takes half of what's there.
        let g = Geom::tiles(600.0);
        assert_eq!(g.cols, 2);
        assert_eq!(g.cell, [((600.0 - 2.0 * PAD) / 2.0f32).floor(), TILE_H]);
        assert_eq!(Geom::tiles(100.0).cols, 1, "always one column");
        assert_eq!(Geom::tiles(100.0).cell[0], TILE_W, "too narrow: the tile keeps its width (the list clips)");
        assert_eq!(Geom::tiles(f32::MAX).cell[0], TILE_W);
        assert_eq!(ViewMode::Tiles.geom(IconSize::Large, 600.0), Some(g), "one size whatever the icon size");
        assert_eq!(ViewMode::Columns.geom(IconSize::Large, 600.0), None);
        // The 2-D arrows work on it as on the icon view's.
        assert_eq!(step(0, g.cols, 5, 0, 1), 2);
    }
}
