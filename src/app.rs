//! App state and everything that changes it: navigation, selection, filtering, keys.
//! File operations live in `actions.rs`. Every handler lands its change with `tx.instant()`:
//! a file manager should never animate.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use rune::{Key, KeyEvent, Latest, TextEdit, Tx};

use crate::fs::{self, Entry, Listing, Sort, SortKey};
use crate::ops;

/// Rows a Page Up / Page Down moves.
const PAGE: usize = 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Nav {
    New,
    Back,
    Forward,
    Reload,
}

pub(crate) struct Pending {
    pub path: PathBuf,
    pub nav: Nav,
    /// Names to select once the listing lands; the first gets the cursor.
    pub select: Vec<OsString>,
    /// Start renaming the first selected name once it lands (a new folder).
    pub rename: bool,
}

/// An inline rename in progress.
pub struct Rename {
    pub os: OsString,
    pub edit: TextEdit,
}

/// What ⌘C / ⌘X put aside, so a paste knows a cut means move.
pub(crate) struct Clip {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
}

pub struct State {
    pub cwd: PathBuf,
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
    pub back: Vec<PathBuf>,
    pub fwd: Vec<PathBuf>,
    pub places: Vec<(String, PathBuf)>,
    /// Bumped whenever `visible` changes, so virtual rows know their keys moved.
    pub version: u64,
    /// The last error or note, shown in the status bar until the next action.
    pub note: Option<String>,
    pub now: rune::LocalTime,
    pub rename: Option<Rename>,
    /// The running file operation, if any.
    pub job: Option<ops::Running>,
    /// Folder row a file drag is hovering.
    pub drop_target: Option<OsString>,
    pub(crate) clip: Option<Clip>,
    pub(crate) paste_move: bool,
    pub(crate) undo: Vec<ops::Undo>,
    /// The running job is an undo, so its own undo record is dropped.
    pub(crate) undo_pending_skip: bool,
    /// Where the rename field's late writes go after the rename has ended (the runtime
    /// clears the field's focus through its binding a frame after a commit).
    rename_sink: TextEdit,
    pub(crate) job_seen: (u64, usize),
    pub(crate) waker: Option<rune::Waker>,
    loader: Latest<Listing>,
    pending: Option<Pending>,
}

impl State {
    pub fn new() -> State {
        let start = std::env::args_os()
            .skip(1)
            .map(PathBuf::from)
            .find(|p| p.is_dir())
            .or_else(|| std::env::current_dir().ok().filter(|p| p != Path::new("/")))
            .unwrap_or_else(fs::home);
        let sort = Sort { key: SortKey::Name, desc: false };
        // The first listing is read synchronously so the first frame already shows it.
        let first = fs::list(start.clone(), sort);
        let mut s = State {
            cwd: start,
            entries: Vec::new(),
            visible: Vec::new(),
            sel: 0,
            marked: HashSet::new(),
            anchor: 0,
            filter: String::new(),
            show_hidden: false,
            sort,
            back: Vec::new(),
            fwd: Vec::new(),
            places: fs::places(),
            version: 0,
            note: None,
            now: now(),
            rename: None,
            job: None,
            drop_target: None,
            clip: None,
            paste_move: false,
            undo: Vec::new(),
            undo_pending_skip: false,
            rename_sink: TextEdit::new(""),
            job_seen: (0, 0),
            waker: None,
            loader: Latest::default(),
            pending: None,
        };
        match first.result {
            Ok(v) => s.entries = v,
            Err(e) => s.note = Some(e),
        }
        s.refilter(None);
        s
    }

    pub fn setup(&mut self, waker: rune::Waker) {
        self.loader.set_waker(waker.clone());
        self.waker = Some(waker);
    }

    pub fn rename_edit(&mut self) -> &mut TextEdit {
        match &mut self.rename {
            Some(r) => &mut r.edit,
            None => &mut self.rename_sink,
        }
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

    pub fn path_mode(&self) -> bool {
        is_path_like(&self.filter)
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    // ---- navigation -------------------------------------------------------------------------

    pub(crate) fn go_full(&mut self, path: PathBuf, nav: Nav, select: Vec<OsString>, rename: bool) {
        let sort = self.sort;
        self.rename = None;
        self.pending = Some(Pending { path: path.clone(), nav, select, rename });
        self.loader.request(path, move |p| fs::list(p, sort));
    }

    pub fn go(&mut self, path: PathBuf, nav: Nav, select: Option<OsString>) {
        self.go_full(path, nav, select.into_iter().collect(), false);
    }

    pub fn go_to(&mut self, path: PathBuf) {
        if path != self.cwd {
            self.go(path, Nav::New, None);
        }
    }

    pub fn parent(&mut self) {
        if let Some(p) = self.cwd.parent() {
            let came_from = self.cwd.file_name().map(|n| n.to_os_string());
            self.go(p.to_path_buf(), Nav::New, came_from);
        }
    }

    pub fn go_back(&mut self) {
        if let Some(p) = self.back.last().cloned() {
            let came_from = self.cwd.file_name().map(|n| n.to_os_string());
            let select = if self.cwd.parent() == Some(p.as_path()) { came_from } else { None };
            self.go(p, Nav::Back, select);
        }
    }

    pub fn go_forward(&mut self) {
        if let Some(p) = self.fwd.last().cloned() {
            self.go(p, Nav::Forward, None);
        }
    }

    /// Re-reads the folder, keeping the selection (or selecting `names` instead).
    pub fn reload_selecting(&mut self, names: Vec<OsString>, rename: bool) {
        let keep = if names.is_empty() { self.targets().iter().map(|e| e.os.clone()).collect() } else { names };
        self.go_full(self.cwd.clone(), Nav::Reload, keep, rename);
    }

    pub fn reload(&mut self) {
        self.reload_selecting(Vec::new(), false);
    }

    /// Opens the cursor's entry: a folder navigates, a file opens in its default app.
    /// With several files selected, opens each file.
    pub fn open_selected(&mut self) {
        let targets: Vec<(PathBuf, bool)> = self.targets().iter().map(|e| (self.cwd.join(&e.os), e.is_dir)).collect();
        if let [(path, true)] = targets.as_slice() {
            let p = path.clone();
            self.go(p, Nav::New, None);
            return;
        }
        for (path, is_dir) in targets {
            if is_dir {
                continue;
            }
            if let Err(err) = fs::open(&path) {
                self.note = Some(format!("Couldn't open: {err}"));
            }
        }
    }

    pub fn reveal_selected(&mut self) {
        let path = self.selected().map_or_else(|| self.cwd.clone(), |e| self.cwd.join(&e.os));
        if let Err(err) = fs::reveal(&path) {
            self.note = Some(format!("Couldn't reveal: {err}"));
        }
    }

    /// Called from `poll`: lands a finished listing or file operation. True when the view
    /// needs to change.
    pub fn poll(&mut self, tx: &mut Tx) -> bool {
        tx.instant();
        let job = self.poll_job();
        let Some(listing) = self.loader.take() else { return job };
        let Some(p) = self.pending.take_if(|p| p.path == listing.path) else { return job };
        match listing.result {
            Err(e) => {
                self.note = Some(format!("{}: {e}", fs::pretty(&p.path)));
            }
            Ok(entries) => {
                let old = std::mem::replace(&mut self.cwd, p.path);
                let moved = old != self.cwd;
                match p.nav {
                    Nav::New => {
                        self.back.push(old);
                        self.fwd.clear();
                    }
                    Nav::Back => {
                        self.back.pop();
                        self.fwd.push(old);
                    }
                    Nav::Forward => {
                        self.fwd.pop();
                        self.back.push(old);
                    }
                    Nav::Reload => {}
                }
                self.entries = entries;
                self.now = now();
                if p.nav != Nav::Reload {
                    self.filter.clear();
                    self.note = None;
                }
                self.marked.clear();
                if p.select.len() > 1 {
                    self.marked.extend(p.select.iter().cloned());
                }
                self.refilter(p.select.first().map(OsString::as_os_str));
                self.anchor = self.sel;
                tx.set_title(title(&self.cwd));
                if moved {
                    tx.scroll_to_instant("list", 0.0, 0.0);
                }
                self.reveal(tx);
                if p.rename {
                    self.start_rename();
                }
            }
        }
        true
    }

    // ---- selection, sorting, filtering ------------------------------------------------------

    /// Recomputes `visible`. Keeps `select` (by name) as the cursor when it's still shown;
    /// otherwise the first name starting with the filter, else the first row.
    pub fn refilter(&mut self, select: Option<&OsStr>) {
        let q = if self.path_mode() { String::new() } else { self.filter.to_lowercase() };
        let hidden = self.show_hidden;
        self.visible.clear();
        self.visible.extend(
            self.entries
                .iter()
                .enumerate()
                .filter(|(_, e)| (hidden || !e.hidden) && (q.is_empty() || e.lower.contains(&q)))
                .map(|(i, _)| i as u32),
        );
        let by_name = select.and_then(|n| self.visible.iter().position(|&i| self.entries[i as usize].os == n));
        let by_prefix = || {
            (!q.is_empty()).then(|| self.visible.iter().position(|&i| self.entries[i as usize].lower.starts_with(&q))).flatten()
        };
        self.sel = by_name.or_else(by_prefix).unwrap_or(0);
        self.version += 1;
    }

    fn cursor_name(&self) -> Option<OsString> {
        self.selected().map(|e| e.os.clone())
    }

    pub fn set_sort(&mut self, key: SortKey) {
        self.sort = if self.sort.key == key {
            Sort { key, desc: !self.sort.desc }
        } else {
            // Newest and largest first feel right; names and kinds read A→Z.
            Sort { key, desc: matches!(key, SortKey::Size | SortKey::Modified) }
        };
        let keep = self.cursor_name();
        fs::sort_entries(&mut self.entries, self.sort);
        self.refilter(keep.as_deref());
    }

    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        let keep = self.cursor_name();
        self.refilter(keep.as_deref());
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
        if self.marked.is_empty() {
            if let Some(n) = self.cursor_name() {
                self.marked.insert(n);
            }
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

    fn step(&mut self, by: isize, extend: bool, tx: &mut Tx) {
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
            tx.scroll_into_view_in("rows", e.key.clone());
        }
    }

    fn type_filter(&mut self, text: &str) {
        let keep = self.cursor_name();
        self.filter.push_str(text);
        self.marked.clear();
        self.refilter(if self.path_mode() { keep.as_deref() } else { None });
    }

    fn submit_path(&mut self) {
        let path = fs::expand(self.filter.trim());
        if path.is_dir() {
            self.go(path, Nav::New, None);
        } else if path.exists() {
            if let Err(e) = fs::open(&path) {
                self.note = Some(format!("Couldn't open: {e}"));
            }
        } else {
            self.note = Some(format!("No such folder: {}", path.display()));
        }
    }

    // ---- keys -------------------------------------------------------------------------------

    pub fn on_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        tx.instant();
        let m = ev.mods;
        // While renaming, the field has the keys; only Escape (which it passes on) gets here.
        if self.rename.is_some() {
            if ev.key == Key::Escape {
                self.rename = None;
                tx.blur();
            }
            return true;
        }
        match &ev.key {
            Key::Up if m.cmd || m.alt => self.parent(),
            Key::Down if m.cmd || m.alt => self.open_selected(),
            Key::Up => self.step(-1, m.shift, tx),
            Key::Down => self.step(1, m.shift, tx),
            Key::PageUp => self.step(-(PAGE as isize), m.shift, tx),
            Key::PageDown => self.step(PAGE as isize, m.shift, tx),
            Key::Home => self.step(isize::MIN / 2, m.shift, tx),
            Key::End => self.step(isize::MAX / 2, m.shift, tx),
            Key::Left if m.alt => self.go_back(),
            Key::Right if m.alt => self.go_forward(),
            Key::Left => self.parent(),
            Key::Right => {
                if self.selected().is_some_and(|e| e.is_dir) {
                    self.select(self.sel);
                    self.open_selected();
                }
            }
            Key::Enter if m.cmd => self.reveal_selected(),
            Key::Enter if self.path_mode() => self.submit_path(),
            Key::Enter => self.open_selected(),
            Key::Function(2) => self.start_rename(),
            Key::Escape => {
                if let Some(job) = &self.job {
                    job.cancel();
                    self.note = Some("Cancelling…".into());
                } else if !self.filter.is_empty() || self.note.is_some() || !self.marked.is_empty() {
                    self.note = None;
                    self.marked.clear();
                    let keep = self.cursor_name();
                    self.filter.clear();
                    self.refilter(keep.as_deref());
                    self.reveal(tx);
                } else {
                    return false;
                }
            }
            // ⌘⌫ (Finder) or Delete: move to Trash.
            Key::Backspace if m.cmd => self.trash_selected(),
            Key::Delete => self.trash_selected(),
            Key::Backspace if m.alt => {
                self.filter.clear();
                self.refilter(None);
            }
            Key::Backspace => {
                if self.filter.pop().is_some() {
                    let keep = self.cursor_name();
                    self.refilter(keep.as_deref());
                    self.reveal(tx);
                } else {
                    self.parent();
                }
            }
            Key::Command(c) => return self.command(*c, m, tx),
            Key::Text(t) if !m.cmd && !m.ctrl => {
                // Space with nothing typed is free (reserved for preview later).
                if t == " " && self.filter.is_empty() {
                    return false;
                }
                self.type_filter(t);
                self.reveal(tx);
            }
            _ => return false,
        }
        true
    }

    fn command(&mut self, c: char, m: rune::Mods, tx: &mut Tx) -> bool {
        match c {
            '[' => self.go_back(),
            ']' => self.go_forward(),
            '.' | '>' => self.toggle_hidden(),
            'r' if m.shift => self.start_rename(),
            'r' => self.reload(),
            'o' => self.open_selected(),
            'a' => self.select_all(),
            'c' if m.shift || m.alt => self.copy_path(tx),
            'c' => self.copy_selected(false, tx),
            'x' => self.copy_selected(true, tx),
            // Plain ⌘V reaches the root's paste handler on its own; ⌥⌘V moves (as in Finder).
            'v' => {
                self.paste_move = m.alt;
                tx.request_paste();
            }
            'd' => self.duplicate(),
            'n' if m.shift => self.new_folder(),
            'z' => self.undo_last(),
            'l' => {
                let mut p = fs::pretty(&self.cwd);
                if !p.ends_with(std::path::MAIN_SEPARATOR) {
                    p.push(std::path::MAIN_SEPARATOR);
                }
                self.filter = p;
                self.refilter(None);
            }
            d @ '1'..='9' => {
                let i = d as usize - '1' as usize;
                if let Some((_, p)) = self.places.get(i).cloned() {
                    self.go_to(p);
                }
            }
            _ => return false,
        }
        true
    }
}

pub fn is_path_like(s: &str) -> bool {
    let b = s.as_bytes();
    s.starts_with('/')
        || s.starts_with('~')
        || s.starts_with('\\')
        || (cfg!(windows) && b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

pub fn title(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

fn now() -> rune::LocalTime {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    rune::LocalTime::at(ms)
}
