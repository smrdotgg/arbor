//! The list view's columns after the name: which are shown and how wide, for every window, the
//! list and a search's results alike. It's user data (NORTHSTAR: "User data is not a cache"),
//! kept in `columns.toml` beside the settings file, not in `config.toml`, which people edit by
//! hand and Arbor never rewrites.
//!
//! - **Columns:** Kind, Size and Date Modified are shown to start with; Extension, Date Created
//!   and Permissions can be. Every one of them comes from what the listing reads anyway (one
//!   `lstat`/`statx` per entry, or Windows' directory record), so showing one reads nothing
//!   more. A search's results also have their Folder column, which isn't optional.
//! - **Showing and hiding:** the header row's context menu (a check beside each shown column),
//!   or the palette ("Show Date Created Column"…). "Reset Columns" goes back to the defaults.
//! - **Widths:** a column's left edge in the header drags its width; the name takes what's
//!   left, never less than [`NAME_MIN`]. A pane too narrow for every shown column leaves out
//!   the least needed first (Permissions, Date Created, Extension, Kind, then Date Modified),
//!   as it left out Kind and then the date before; Size always stays while it's shown.
//! - **No file:** the defaults. **Read** once at startup by the startup read, with the
//!   settings (never the window's thread); a file that can't be read or isn't valid gives the
//!   defaults and a note, and the next change replaces it. **Written** by one thread of its
//!   own ([`Writer`]), started by the first change: the whole file each time (a temporary file
//!   renamed over it), the last of several changes in a row only. A failed write is said once
//!   in the status bar. Quitting gives it up to 100 ms to finish.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use crate::commands::Cmd;
use crate::fs::{self, SortKey};
use crate::persist;

/// The file's name, beside `config.toml`.
pub const FILE: &str = "columns.toml";

/// The narrowest the name column gets: a column that would leave it less isn't shown, and a
/// drag can't widen a column past it.
pub const NAME_MIN: f32 = 180.0;
/// The least a search's Folder column gets beside the name.
pub const FOLDER_MIN: f32 = 96.0;
/// The space between columns, and the row's padding on each side.
pub const GAP: f32 = 12.0;
pub const PAD: f32 = 12.0;
/// The widest a drag makes a column.
pub const MAX_W: f32 = 480.0;

/// Where the columns are kept.
pub fn file_path() -> Option<PathBuf> {
    Some(crate::config::path()?.with_file_name(FILE))
}

/// An optional column of the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Field {
    Kind,
    Extension,
    Size,
    Modified,
    Created,
    Permissions,
}

impl Field {
    /// Every column, left to right as shown.
    pub const ALL: [Field; 6] = [Field::Kind, Field::Extension, Field::Size, Field::Modified, Field::Created, Field::Permissions];

    fn ix(self) -> usize {
        self as usize
    }

    /// Its name in the file.
    pub fn id(self) -> &'static str {
        match self {
            Field::Kind => "kind",
            Field::Extension => "extension",
            Field::Size => "size",
            Field::Modified => "modified",
            Field::Created => "created",
            Field::Permissions => "permissions",
        }
    }

    pub fn from_id(id: &str) -> Option<Field> {
        Field::ALL.into_iter().find(|f| f.id() == id)
    }

    /// Its name in menus and the palette.
    pub fn title(self) -> &'static str {
        match self {
            Field::Kind => "Kind",
            Field::Extension => "Extension",
            Field::Size => "Size",
            Field::Modified => "Date Modified",
            Field::Created => "Date Created",
            Field::Permissions => {
                if cfg!(windows) {
                    "Attributes"
                } else {
                    "Permissions"
                }
            }
        }
    }

    /// What its header says.
    pub fn header(self) -> &'static str {
        match self {
            Field::Extension => "Ext.",
            Field::Modified => "Modified",
            Field::Created => "Created",
            f => f.title(),
        }
    }

    pub fn default_w(self) -> f32 {
        match self {
            Field::Kind => 72.0,
            Field::Extension => 56.0,
            Field::Size => 80.0,
            Field::Modified | Field::Created => 132.0,
            Field::Permissions => {
                if cfg!(windows) {
                    72.0
                } else {
                    88.0
                }
            }
        }
    }

    /// The narrowest a drag makes it.
    pub fn min_w(self) -> f32 {
        match self {
            Field::Kind | Field::Extension => 36.0,
            Field::Size => 48.0,
            Field::Modified | Field::Created => 72.0,
            Field::Permissions => 48.0,
        }
    }

    fn default_on(self) -> bool {
        matches!(self, Field::Kind | Field::Size | Field::Modified)
    }

    /// Which goes first when a pane is too narrow for every shown column: the lowest.
    fn keep_rank(self) -> u8 {
        match self {
            Field::Permissions => 0,
            Field::Created => 1,
            Field::Extension => 2,
            Field::Kind => 3,
            Field::Modified => 4,
            Field::Size => 5,
        }
    }

    /// What a click on its header sorts by. The extension sorts as Kind does (by extension,
    /// then name): the same order, so both headers show it.
    pub fn sort_key(self) -> Option<SortKey> {
        match self {
            Field::Kind | Field::Extension => Some(SortKey::Kind),
            Field::Size => Some(SortKey::Size),
            Field::Modified => Some(SortKey::Modified),
            Field::Created => Some(SortKey::Created),
            Field::Permissions => None,
        }
    }

    /// Right-aligned (numbers).
    pub fn end_aligned(self) -> bool {
        self == Field::Size
    }

    /// The palette command that shows or hides it.
    pub fn cmd(self) -> Cmd {
        match self {
            Field::Kind => Cmd::ColumnKind,
            Field::Extension => Cmd::ColumnExtension,
            Field::Size => Cmd::ColumnSize,
            Field::Modified => Cmd::ColumnModified,
            Field::Created => Cmd::ColumnCreated,
            Field::Permissions => Cmd::ColumnPermissions,
        }
    }

    pub fn of_cmd(cmd: Cmd) -> Option<Field> {
        Field::ALL.into_iter().find(|f| f.cmd() == cmd)
    }

    /// Its cell's text for `e` (`now` for dates), as the row shows it.
    pub fn cell(self, e: &fs::Entry, now: rune::LocalTime) -> std::borrow::Cow<'_, str> {
        use std::borrow::Cow;
        match self {
            Field::Kind => Cow::Borrowed(fs::kind(e)),
            Field::Extension => Cow::Borrowed(fs::shown_ext(e)),
            Field::Size if e.is_dir => Cow::Borrowed("—"),
            Field::Size => Cow::Owned(fs::fmt_size(e.size)),
            Field::Modified => Cow::Owned(fs::fmt_time(e.mtime_ms, now)),
            Field::Created => match e.born_ms() {
                0 => Cow::Borrowed("—"),
                ms => Cow::Owned(fs::fmt_time(ms, now)),
            },
            Field::Permissions => Cow::Owned(fs::fmt_mode(e)),
        }
    }
}

/// Which columns are shown, and each one's width (kept while hidden).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ListCols {
    on: [bool; 6],
    w: [f32; 6],
}

impl Default for ListCols {
    fn default() -> ListCols {
        ListCols { on: Field::ALL.map(Field::default_on), w: Field::ALL.map(Field::default_w) }
    }
}

impl ListCols {
    pub fn shown(&self, f: Field) -> bool {
        self.on[f.ix()]
    }

    pub fn width(&self, f: Field) -> f32 {
        self.w[f.ix()]
    }

    pub fn set_shown(&mut self, f: Field, on: bool) {
        self.on[f.ix()] = on;
    }

    /// Sets `f`'s width, within its limits, in whole points.
    pub fn set_width(&mut self, f: Field, w: f32) {
        self.w[f.ix()] = clamp_w(f, w);
    }

    /// The shown columns that fit a pane `pane_w` wide, with their widths: what's left after
    /// them is the name's (and a search's Folder column's, `folder`), at least [`NAME_MIN`]
    /// (and [`FOLDER_MIN`]). Before the pane's first layout (`f32::MAX`) all of them.
    pub fn fit(&self, pane_w: f32, folder: bool) -> Fit {
        let mut fit = Fit { w: [None; 6], folder };
        for f in Field::ALL {
            if self.shown(f) {
                fit.w[f.ix()] = Some(self.width(f));
            }
        }
        // The least needed go first; Size stays whatever the width (as it always has).
        let mut order: Vec<Field> = Field::ALL.into_iter().filter(|&f| self.shown(f) && f != Field::Size).collect();
        order.sort_by_key(|f| f.keep_rank());
        let mut drop = order.into_iter();
        while fit.slack(pane_w) < 0.0 {
            match drop.next() {
                Some(f) => fit.w[f.ix()] = None,
                None => break,
            }
        }
        fit
    }
}

/// `w` within `f`'s limits, rounded to a whole point.
fn clamp_w(f: Field, w: f32) -> f32 {
    if !w.is_finite() {
        return f.default_w();
    }
    w.clamp(f.min_w(), MAX_W).round()
}

/// The columns a pane shows ([`ListCols::fit`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    w: [Option<f32>; 6],
    /// A search's Folder column is beside the name.
    pub folder: bool,
}

impl Fit {
    pub fn get(&self, f: Field) -> Option<f32> {
        self.w[f.ix()]
    }

    /// The columns shown, left to right, with their widths.
    pub fn iter(&self) -> impl Iterator<Item = (Field, f32)> + '_ {
        Field::ALL.into_iter().filter_map(|f| self.get(f).map(|w| (f, w)))
    }

    /// Room the columns after the name take, with the gap before each and the row's padding.
    pub fn fixed(&self) -> f32 {
        2.0 * PAD + self.iter().map(|(_, w)| w + GAP).sum::<f32>() + if self.folder { GAP } else { 0.0 }
    }

    /// How much room past the least the name (and Folder) need is left in a pane `pane_w`
    /// wide: what a column can still grow by. Negative when they don't fit.
    pub fn slack(&self, pane_w: f32) -> f32 {
        if pane_w == f32::MAX {
            return f32::MAX;
        }
        pane_w - self.fixed() - NAME_MIN - if self.folder { FOLDER_MIN } else { 0.0 }
    }
}

/// A column's width as its left edge is dragged `dx` points (right narrows it) from where it
/// was, `start`: within its limits, and never taking more than `slack` from the name.
pub fn dragged(f: Field, start: f32, dx: f32, slack: f32) -> f32 {
    let w = clamp_w(f, start - dx);
    if w > start { w.min(start + slack.max(0.0).floor()).max(start) } else { w }
}

// ---- the file -----------------------------------------------------------------------------

/// What the startup read found: the columns (the defaults when there's no file or it's bad),
/// and a problem to show.
#[derive(Debug, Default, PartialEq)]
pub struct Loaded {
    pub cols: ListCols,
    pub error: Option<String>,
}

/// Reads the columns file at `path` (startup, off the window's thread).
pub fn load(path: Option<&Path>) -> Loaded {
    let Some(path) = path else { return Loaded::default() };
    let bad = |why: &str| Loaded {
        cols: ListCols::default(),
        error: Some(format!(
            "Columns: couldn't use {} ({why}); showing the default columns. Your next change to them replaces it",
            fs::pretty(path)
        )),
    };
    match crate::config::read_text(path) {
        Ok(None) => Loaded::default(),
        Ok(Some(text)) => match parse(&text) {
            Ok(cols) => Loaded { cols, error: None },
            Err(e) => bad(&e),
        },
        Err(e) => bad(&e),
    }
}

/// The columns in `text`: `shown = ["kind", …]` and a `[width]` table. Names Arbor doesn't
/// know (from a newer version) are left out; widths are kept within their limits. A missing
/// `shown` or width is the default.
pub fn parse(text: &str) -> Result<ListCols, String> {
    let doc = toml_edit::Document::parse(text.to_string()).map_err(|e| {
        let at = e.span().map_or(0, |s| s.start).min(text.len());
        let line = text.as_bytes()[..at].iter().filter(|&&b| b == b'\n').count() + 1;
        format!("line {line}: {}", e.message().lines().next().unwrap_or("not valid TOML").trim().trim_end_matches('.'))
    })?;
    let root = doc.as_table();
    let mut cols = ListCols::default();
    if let Some(item) = root.get("shown") {
        let Some(arr) = item.as_array() else { return Err("shown isn't a list".into()) };
        cols.on = [false; 6];
        for v in arr.iter() {
            let Some(id) = v.as_str() else { return Err("shown has something that isn't a column's name".into()) };
            if let Some(f) = Field::from_id(id) {
                cols.set_shown(f, true);
            }
        }
    }
    if let Some(item) = root.get("width") {
        let Some(t) = item.as_table_like() else { return Err("width isn't a table".into()) };
        for (id, v) in t.iter() {
            let n = v.as_integer().map(|i| i as f64).or_else(|| v.as_float());
            let Some(n) = n else { return Err(format!("the width of {id} isn't a number")) };
            if let Some(f) = Field::from_id(id) {
                cols.set_width(f, n as f32);
            }
        }
    }
    Ok(cols)
}

/// The file's text for `cols`.
pub fn serialize(cols: &ListCols) -> String {
    let shown: Vec<String> = Field::ALL.into_iter().filter(|&f| cols.shown(f)).map(|f| format!("\"{}\"", f.id())).collect();
    let mut s = String::from(
        "# Arbor's list columns: which are shown, and how wide they are (points). Arbor rewrites this\n\
         # file when you show, hide or resize a column. Delete it to go back to the defaults.\n",
    );
    s.push_str(&format!("shown = [{}]\n\n[width]\n", shown.join(", ")));
    for f in Field::ALL {
        s.push_str(&format!("{} = {}\n", f.id(), cols.width(f)));
    }
    s
}

// ---- the writer ---------------------------------------------------------------------------

enum Msg {
    Save(ListCols),
    /// Answered once everything sent before it is written.
    Flush(Sender<()>),
}

/// The one thread that writes `columns.toml`, for every window: the whole file, the last of
/// the changes queued, renamed over the old one.
pub struct Writer {
    tx: Sender<Msg>,
    pub(crate) errors: Receiver<String>,
}

impl Writer {
    pub(crate) fn spawn(path: PathBuf, waker: Option<rune::Waker>) -> Writer {
        let (tx, rx) = std::sync::mpsc::channel::<Msg>();
        let (mut failures, errors) = persist::Failures::new(waker);
        persist::at_exit(&tx, Msg::Flush);
        std::thread::Builder::new()
            .name("arbor-columns".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    let (mut last, mut replies) = (None, Vec::new());
                    for m in std::iter::once(first).chain(std::iter::from_fn(|| rx.try_recv().ok())) {
                        match m {
                            Msg::Save(c) => last = Some(c),
                            Msg::Flush(r) => replies.push(r),
                        }
                    }
                    if let Some(c) = last {
                        let written = persist::replace(&path, serialize(&c).as_bytes(), persist::Durability::Plain);
                        failures.note(written, |e| format!("Couldn't save the columns ({}): {e}", fs::pretty(&path)));
                    }
                    for r in replies {
                        let _ = r.send(());
                    }
                }
            })
            .expect("can't start the columns' writer");
        Writer { tx, errors }
    }

    pub(crate) fn save(&self, cols: ListCols) {
        let _ = self.tx.send(Msg::Save(cols));
    }

    /// Waits up to `wait` for what's queued to be written. True when it was.
    #[cfg(test)]
    pub fn flush(&self, wait: std::time::Duration) -> bool {
        persist::flush(&self.tx, wait, Msg::Flush)
    }
}

// ---- the window's side --------------------------------------------------------------------

impl crate::app::State {
    /// The list's columns, as every window shows them.
    pub fn cols(&self) -> ListCols {
        self.shared.cols.get()
    }

    /// Shows or hides column `f` (the header's menu, the palette), saying which.
    pub fn toggle_column(&mut self, f: Field) {
        let mut c = self.cols();
        let on = !c.shown(f);
        c.set_shown(f, on);
        self.change_columns(c);
        self.note = Some(format!("{} column {}", f.title(), if on { "shown" } else { "hidden" }));
    }

    /// "Reset Columns": the default columns at their default widths.
    pub fn reset_columns(&mut self) {
        self.change_columns(ListCols::default());
        self.note = Some("Columns reset to the defaults".into());
    }

    /// The columns are now `c`, in every window, and saved.
    pub(crate) fn change_columns(&mut self, c: ListCols) {
        self.shared.cols.set(c);
        self.shared.cols_set.set(true);
        self.save_columns();
        self.tell_columns();
    }

    /// The other windows draw the columns again (they read them from `Shared`).
    fn tell_columns(&self) {
        self.shared.tell_others(self.win, || Box::new(|_, _| {}));
    }

    /// Hands the columns to their writer (started by the first change), which wakes the open
    /// window focused last if it fails.
    fn save_columns(&self) {
        let Some(path) = self.shared.cols_file.borrow().clone() else { return };
        let waker = self.waker.as_ref().map(|_| self.shared.live_waker());
        let mut w = self.shared.cols_writer.borrow_mut();
        w.get_or_insert_with(|| Writer::spawn(path, waker)).save(self.cols());
    }

    /// The writer's failure, said once, from `poll`. True when there was one.
    pub(crate) fn poll_cols_writer(&mut self) -> bool {
        let error = self.shared.cols_writer.borrow().as_ref().and_then(|w| w.errors.try_recv().ok());
        let Some(e) = error else { return false };
        self.note = Some(e);
        self.alert = self.note.clone();
        true
    }

    /// A drag of column `f`'s left edge in the header: `slack` is what the name could give
    /// when it was drawn. The width follows the pointer; it's saved when the drag ends, and
    /// put back when it's cancelled (Esc).
    pub fn drag_header(&mut self, f: Field, phase: rune::Phase, dx: f32, slack: f32, cancelled: bool) {
        match phase {
            rune::Phase::Down => self.head_drag = Some((f, self.cols().width(f), slack)),
            _ if cancelled => {
                if let Some((f, start, _)) = self.head_drag.take() {
                    let mut c = self.cols();
                    c.set_width(f, start);
                    self.shared.cols.set(c);
                }
            }
            rune::Phase::Up => {
                if let Some((f, start, _)) = self.head_drag.take()
                    && self.cols().width(f) != start
                {
                    let c = self.cols();
                    self.change_columns(c);
                }
            }
            _ => {
                if let Some((f, start, slack)) = self.head_drag {
                    let mut c = self.cols();
                    c.set_width(f, dragged(f, start, dx, slack));
                    self.shared.cols.set(c);
                }
            }
        }
    }

    /// Waits up to `wait` for the columns' writer (tests).
    #[cfg(test)]
    pub(crate) fn flush_columns_file(&self, wait: std::time::Duration) -> bool {
        self.shared.cols_writer.borrow().as_ref().is_some_and(|w| w.flush(wait))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_kind_size_and_modified() {
        let c = ListCols::default();
        let on: Vec<Field> = Field::ALL.into_iter().filter(|&f| c.shown(f)).collect();
        assert_eq!(on, [Field::Kind, Field::Size, Field::Modified]);
        // As the list's columns were before they could change.
        assert_eq!((c.width(Field::Kind), c.width(Field::Size), c.width(Field::Modified)), (72.0, 80.0, 132.0));
    }

    #[test]
    fn the_file_round_trips() {
        let mut c = ListCols::default();
        c.set_shown(Field::Kind, false);
        c.set_shown(Field::Created, true);
        c.set_shown(Field::Permissions, true);
        c.set_width(Field::Created, 150.4);
        c.set_width(Field::Size, 64.0);
        let text = serialize(&c);
        assert_eq!(parse(&text), Ok(c), "{text}");
        assert_eq!(c.width(Field::Created), 150.0, "whole points");
        // Everything hidden is a valid choice too (the name stays).
        let mut none = ListCols::default();
        for f in Field::ALL {
            none.set_shown(f, false);
        }
        assert_eq!(parse(&serialize(&none)), Ok(none));
    }

    #[test]
    fn widths_stay_within_limits_and_unknown_names_are_left_out() {
        let c = parse("shown = [\"size\", \"colour\"]\n[width]\nsize = 5\nkind = 9000\nmodified = 140.4\nfuture = 3\n").unwrap();
        assert!(c.shown(Field::Size) && !c.shown(Field::Kind) && !c.shown(Field::Modified));
        assert_eq!(c.width(Field::Size), Field::Size.min_w());
        assert_eq!(c.width(Field::Kind), MAX_W);
        assert_eq!(c.width(Field::Modified), 140.0, "rounded");
        // A file with neither is the defaults.
        assert_eq!(parse("# nothing\n"), Ok(ListCols::default()));
    }

    #[test]
    fn a_bad_file_is_an_error_not_a_guess() {
        for (text, why) in [
            ("shown = \"kind\"\n", "shown isn't a list"),
            ("shown = [1]\n", "shown has something that isn't a column's name"),
            ("width = 3\n", "width isn't a table"),
            ("[width]\nkind = \"wide\"\n", "the width of kind isn't a number"),
        ] {
            assert_eq!(parse(text), Err(why.to_string()), "{text}");
        }
        assert!(parse("shown = [\n").unwrap_err().starts_with("line "), "a syntax error names its line");
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-listcols-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join(FILE)
    }

    /// A corrupt or unreadable file means the defaults, and a note saying the next change
    /// replaces it; no file means the defaults and nothing to say.
    #[test]
    fn a_corrupt_file_means_the_defaults_and_a_note() {
        let p = temp("corrupt");
        assert_eq!(load(Some(&p)), Loaded::default(), "no file");
        std::fs::write(&p, "shown = [\"kind\"\n").unwrap();
        let l = load(Some(&p));
        assert_eq!(l.cols, ListCols::default());
        let e = l.error.unwrap();
        assert!(e.starts_with("Columns: couldn't use") && e.contains("showing the default columns"), "{e}");
        std::fs::write(&p, b"\xff\xfe").unwrap();
        assert!(load(Some(&p)).error.is_some(), "not text");
        std::fs::write(&p, serialize(&ListCols::default())).unwrap();
        assert_eq!(load(Some(&p)), Loaded::default());
    }

    /// The writer writes the last of the changes sent, the whole file, and leaves no
    /// temporary file; a change over a corrupt file replaces it.
    #[test]
    fn the_writer_saves_the_last_change() {
        let p = temp("writer");
        std::fs::write(&p, "corrupt [").unwrap();
        let w = Writer::spawn(p.clone(), None);
        let mut c = ListCols::default();
        for width in [100.0, 110.0, 120.0] {
            c.set_width(Field::Kind, width);
            w.save(c);
        }
        assert!(w.flush(std::time::Duration::from_secs(5)));
        assert_eq!(load(Some(&p)), Loaded { cols: c, error: None });
        assert_eq!(std::fs::read_dir(p.parent().unwrap()).unwrap().count(), 1, "no temporary file left");
    }

    #[test]
    fn a_failed_save_is_said_once() {
        let p = temp("fails");
        // A folder where the file goes: the rename over it fails.
        std::fs::create_dir_all(p.join("x")).unwrap();
        let w = Writer::spawn(p.clone(), None);
        w.save(ListCols::default());
        w.save(ListCols::default());
        assert!(w.flush(std::time::Duration::from_secs(5)));
        let mut c = ListCols::default();
        c.set_shown(Field::Created, true);
        w.save(c);
        assert!(w.flush(std::time::Duration::from_secs(5)));
        let said: Vec<String> = w.errors.try_iter().collect();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].starts_with("Couldn't save the columns"), "{said:?}");
    }

    /// A narrow pane leaves out the least needed columns first; Size stays; the name always
    /// keeps its room once something was left out.
    #[test]
    fn a_narrow_pane_leaves_out_the_least_needed_first() {
        let mut c = ListCols::default();
        for f in Field::ALL {
            c.set_shown(f, true);
        }
        let all = c.fit(f32::MAX, false);
        assert_eq!(all.iter().count(), 6, "before the first layout");
        let full: f32 = 2.0 * PAD + NAME_MIN + Field::ALL.iter().map(|&f| c.width(f) + GAP).sum::<f32>();
        assert_eq!(c.fit(full, false).iter().count(), 6);
        let order = |w: f32| c.fit(w, false).iter().map(|(f, _)| f).collect::<Vec<_>>();
        assert_eq!(order(full - 1.0), [Field::Kind, Field::Extension, Field::Size, Field::Modified, Field::Created]);
        assert_eq!(order(full - c.width(Field::Permissions) - GAP - 1.0), [Field::Kind, Field::Extension, Field::Size, Field::Modified]);
        assert_eq!(order(300.0), [Field::Size], "Size stays");
        for w in [200.0, 400.0, 500.0, 700.0, full - 1.0, full] {
            let fit = c.fit(w, false);
            assert!(fit.slack(w) >= 0.0 || fit.iter().map(|(f, _)| f).eq([Field::Size]), "{w}: {fit:?}");
        }
        // The defaults as the list had them: Kind goes first, then the date.
        let d = ListCols::default();
        let base = 2.0 * PAD + NAME_MIN + GAP + 80.0;
        assert_eq!(d.fit(base + GAP + 132.0, false).iter().map(|(f, _)| f).collect::<Vec<_>>(), [Field::Size, Field::Modified]);
        assert_eq!(d.fit(base + GAP + 131.0, false).iter().map(|(f, _)| f).collect::<Vec<_>>(), [Field::Size]);
        assert_eq!(d.fit(base + 2.0 * GAP + 132.0 + 72.0, false).iter().count(), 3);
        // A search's Folder column needs its room too.
        assert!(d.fit(base + 2.0 * GAP + 132.0 + 72.0, true).iter().count() < 3);
    }

    /// A drag keeps a column within its limits and never takes the name below its least.
    #[test]
    fn resizing_stays_within_limits() {
        let k = Field::Kind;
        assert_eq!(dragged(k, 72.0, -20.0, 100.0), 92.0, "left widens");
        assert_eq!(dragged(k, 72.0, 20.0, 100.0), 52.0, "right narrows");
        assert_eq!(dragged(k, 72.0, 500.0, 100.0), k.min_w());
        assert_eq!(dragged(k, 72.0, -500.0, 10_000.0), MAX_W);
        assert_eq!(dragged(k, 72.0, -50.0, 30.0), 102.0, "only what the name can give");
        assert_eq!(dragged(k, 72.0, -50.0, -5.0), 72.0, "nothing to give: it stays");
        assert_eq!(dragged(k, 72.0, 10.0, -5.0), 62.0, "narrowing always works");
        crate::prop::check("column_drag", 2000, |r| {
            let f = Field::ALL[r.below(6)];
            let start = f.min_w() + r.below(400) as f32;
            let (dx, slack) = (r.below(1200) as f32 - 600.0, r.below(800) as f32 - 100.0);
            let w = dragged(f, start.min(MAX_W), dx, slack);
            assert!(w >= f.min_w() && w <= MAX_W && w == w.round(), "{f:?} {start} {dx} {slack} → {w}");
            assert!(w <= start.min(MAX_W).max(start.min(MAX_W) + slack.max(0.0)), "{f:?} {start} {dx} {slack} → {w}");
        });
    }

    #[test]
    fn every_column_has_a_command_and_an_id() {
        for f in Field::ALL {
            assert_eq!(Field::from_id(f.id()), Some(f));
            assert_eq!(Field::of_cmd(f.cmd()), Some(f));
            assert!(f.min_w() <= f.default_w() && f.default_w() <= MAX_W);
        }
    }
}
