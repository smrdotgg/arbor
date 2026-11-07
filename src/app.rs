//! App state and everything that changes it: navigation, selection, filtering, keys.
//! Every handler lands its change with `tx.instant()`: a file manager should never animate.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use rune::{Key, KeyEvent, Latest, Tx};

use crate::fs::{self, Entry, Listing, Sort, SortKey};

/// Rows a Page Up / Page Down moves.
const PAGE: usize = 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Nav {
    New,
    Back,
    Forward,
    Reload,
}

struct Pending {
    path: PathBuf,
    nav: Nav,
    /// Name to select once the listing lands (the folder we just came out of, say).
    select: Option<OsString>,
}

pub struct State {
    pub cwd: PathBuf,
    /// The whole folder, sorted.
    pub entries: Vec<Entry>,
    /// Indices into `entries` that pass the hidden-file and filter rules, in display order.
    pub visible: Vec<u32>,
    /// Index into `visible`.
    pub sel: usize,
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
            filter: String::new(),
            show_hidden: false,
            sort,
            back: Vec::new(),
            fwd: Vec::new(),
            places: fs::places(),
            version: 0,
            note: None,
            now: now(),
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
        self.loader.set_waker(waker);
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.visible.get(self.sel).map(|&i| &self.entries[i as usize])
    }

    pub fn path_mode(&self) -> bool {
        is_path_like(&self.filter)
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    // ---- navigation -------------------------------------------------------------------------

    pub fn go(&mut self, path: PathBuf, nav: Nav, select: Option<OsString>) {
        let sort = self.sort;
        self.pending = Some(Pending { path: path.clone(), nav, select });
        self.loader.request(path, move |p| fs::list(p, sort));
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

    pub fn reload(&mut self) {
        let keep = self.selected().map(|e| e.os.clone());
        self.go(self.cwd.clone(), Nav::Reload, keep);
    }

    /// Opens the selection: a folder navigates, a file opens in its default app.
    pub fn open_selected(&mut self) {
        let Some(e) = self.selected() else { return };
        let path = self.cwd.join(&e.os);
        if e.is_dir {
            self.go(path, Nav::New, None);
        } else if let Err(err) = fs::open(&path) {
            self.note = Some(format!("Couldn't open: {err}"));
        }
    }

    pub fn reveal_selected(&mut self) {
        let path = self.selected().map_or_else(|| self.cwd.clone(), |e| self.cwd.join(&e.os));
        if let Err(err) = fs::reveal(&path) {
            self.note = Some(format!("Couldn't reveal: {err}"));
        }
    }

    /// Called from `poll`: lands a finished listing. True when the view needs to change.
    pub fn poll(&mut self, tx: &mut Tx) -> bool {
        let Some(listing) = self.loader.take() else { return false };
        let Some(p) = self.pending.take_if(|p| p.path == listing.path) else { return false };
        tx.instant();
        match listing.result {
            Err(e) => {
                self.note = Some(format!("{}: {e}", fs::pretty(&p.path)));
            }
            Ok(entries) => {
                let old = std::mem::replace(&mut self.cwd, p.path);
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
                self.note = None;
                if p.nav != Nav::Reload {
                    self.filter.clear();
                }
                self.refilter(p.select.as_deref());
                tx.set_title(title(&self.cwd));
                tx.scroll_to_instant("list", 0.0, 0.0);
                self.reveal(tx);
            }
        }
        true
    }

    // ---- selection, sorting, filtering ------------------------------------------------------

    /// Recomputes `visible`. Keeps `select` (by name) selected when it's still shown;
    /// otherwise selects the first name starting with the filter, else the first row.
    pub fn refilter(&mut self, select: Option<&std::ffi::OsStr>) {
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

    pub fn set_sort(&mut self, key: SortKey) {
        self.sort = if self.sort.key == key {
            Sort { key, desc: !self.sort.desc }
        } else {
            // Newest and largest first feel right; names and kinds read A→Z.
            Sort { key, desc: matches!(key, SortKey::Size | SortKey::Modified) }
        };
        let keep = self.selected().map(|e| e.os.clone());
        fs::sort_entries(&mut self.entries, self.sort);
        self.refilter(keep.as_deref());
    }

    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        let keep = self.selected().map(|e| e.os.clone());
        self.refilter(keep.as_deref());
    }

    pub fn select(&mut self, i: usize) {
        if !self.visible.is_empty() {
            self.sel = i.min(self.visible.len() - 1);
        }
    }

    fn step(&mut self, by: isize, tx: &mut Tx) {
        if self.visible.is_empty() {
            return;
        }
        let max = self.visible.len() as isize - 1;
        self.sel = (self.sel as isize + by).clamp(0, max) as usize;
        self.reveal(tx);
    }

    pub fn reveal(&self, tx: &mut Tx) {
        if let Some(e) = self.selected() {
            tx.scroll_into_view_in("rows", e.key.clone());
        }
    }

    fn type_filter(&mut self, text: &str) {
        let keep = self.selected().map(|e| e.os.clone());
        self.filter.push_str(text);
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
        match &ev.key {
            Key::Up if m.cmd || m.alt => self.parent(),
            Key::Down if m.cmd || m.alt => self.open_selected(),
            Key::Up => self.step(-1, tx),
            Key::Down => self.step(1, tx),
            Key::PageUp => self.step(-(PAGE as isize), tx),
            Key::PageDown => self.step(PAGE as isize, tx),
            Key::Home => self.step(isize::MIN / 2, tx),
            Key::End => self.step(isize::MAX / 2, tx),
            Key::Left if m.alt => self.go_back(),
            Key::Right if m.alt => self.go_forward(),
            Key::Left => self.parent(),
            Key::Right => {
                if self.selected().is_some_and(|e| e.is_dir) {
                    self.open_selected();
                }
            }
            Key::Enter if m.cmd => self.reveal_selected(),
            Key::Enter if self.path_mode() => self.submit_path(),
            Key::Enter => self.open_selected(),
            Key::Escape => {
                if self.filter.is_empty() && self.note.is_none() {
                    return false;
                }
                self.note = None;
                let keep = self.selected().map(|e| e.os.clone());
                self.filter.clear();
                self.refilter(keep.as_deref());
                self.reveal(tx);
            }
            Key::Backspace if m.cmd || m.alt => {
                if self.filter.is_empty() {
                    self.parent();
                } else {
                    self.filter.clear();
                    self.refilter(None);
                }
            }
            Key::Backspace => {
                if self.filter.pop().is_some() {
                    let keep = self.selected().map(|e| e.os.clone());
                    self.refilter(keep.as_deref());
                    self.reveal(tx);
                } else {
                    self.parent();
                }
            }
            Key::Command(c) => match c {
                '[' => self.go_back(),
                ']' => self.go_forward(),
                '.' | '>' => self.toggle_hidden(),
                'r' => self.reload(),
                'o' => self.open_selected(),
                'c' => {
                    let path = self.selected().map_or_else(|| self.cwd.clone(), |e| self.cwd.join(&e.os));
                    let shown = path.display().to_string();
                    tx.copy(shown.clone());
                    self.note = Some(format!("Copied {shown}"));
                }
                'l' => {
                    let mut p = fs::pretty(&self.cwd);
                    if !p.ends_with(std::path::MAIN_SEPARATOR) {
                        p.push(std::path::MAIN_SEPARATOR);
                    }
                    self.filter = p;
                    self.refilter(None);
                }
                d @ '1'..='9' => {
                    let i = *d as usize - '1' as usize;
                    if let Some((_, p)) = self.places.get(i).cloned() {
                        self.go_to(p);
                    }
                }
                _ => return false,
            },
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
