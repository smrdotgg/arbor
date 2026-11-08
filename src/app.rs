//! App state and everything that changes it: navigation, selection, filtering, keys.
//! File operations live in `actions.rs`. Every handler lands its change with `tx.instant()`:
//! a file manager should never animate.

use std::borrow::Cow;
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use rune::{Key, KeyEvent, Latest, TextEdit, Tx};

use crate::commands::{Cmd, Keymap};
use crate::config::{self, Config, Settings};
use crate::fs::{self, Entry, Listing, Sort, SortKey};
use crate::ops;
use crate::palette::Palette;

/// Rows a Page Up / Page Down moves.
const PAGE: usize = 20;

/// Why a command that starts a file operation can't run while another one does.
const BUSY: &str = "Another file operation is running";

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
    /// Which keys run which command (defaults plus the settings file).
    pub keymap: Keymap,
    /// The settings as last read, so "Reload Settings" applies only what changed.
    settings: Settings,
    pub palette: Option<Palette>,
    /// The palette field's late writes after it closes (see `rename_sink`).
    pub(crate) palette_sink: TextEdit,
    /// Where the rename field's late writes go after the rename has ended (the runtime
    /// clears the field's focus through its binding a frame after a commit).
    rename_sink: TextEdit,
    pub(crate) job_seen: (u64, usize),
    pub(crate) waker: Option<rune::Waker>,
    loader: Latest<Listing>,
    pending: Option<Pending>,
    /// The startup read, while the settings or first listing haven't landed yet.
    boot: Option<Booting>,
}

impl State {
    /// The first window's state. Waits at most [`FIRST_PAINT_WAIT`] for the settings and first
    /// folder (read on the thread `main` started), then opens the window regardless: whatever
    /// isn't in yet lands in `poll` like any other listing.
    pub fn new() -> State {
        let boot = BOOT.lock().ok().and_then(|mut b| b.take()).unwrap_or_else(Booting::start);
        let wait = if rune::tooling::headless_run().is_some() { HEADLESS_WAIT } else { FIRST_PAINT_WAIT };
        State::boot(boot, wait)
    }

    fn boot(boot: Booting, wait: std::time::Duration) -> State {
        let deadline = std::time::Instant::now() + wait;
        let (mut start, mut listing) = (None, None);
        while start.is_none() || listing.is_none() {
            match boot.rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
                Ok(Boot::Start(s)) => start = Some(s),
                Ok(Boot::Listing(l)) => listing = Some(l),
                Err(_) => break,
            }
        }
        // Without the settings yet: the defaults, in the folder Arbor was started in, until
        // they land (`land_start`).
        let (config, cwd, places, note) = match start {
            Some(s) => (s.config, s.start, s.places, s.note),
            None => (Config::default(), std::env::current_dir().unwrap_or_else(|_| fs::home()), Vec::new(), None),
        };
        let mut s = State {
            cwd,
            entries: Vec::new(),
            visible: Vec::new(),
            sel: 0,
            marked: HashSet::new(),
            anchor: 0,
            filter: String::new(),
            show_hidden: config.settings.show_hidden,
            sort: config.settings.sort,
            back: Vec::new(),
            fwd: Vec::new(),
            places,
            version: 0,
            note: config.summary().or(note),
            now: now(),
            rename: None,
            job: None,
            drop_target: None,
            clip: None,
            paste_move: false,
            undo: Vec::new(),
            keymap: config.keymap,
            settings: config.settings,
            palette: None,
            palette_sink: TextEdit::new(""),
            rename_sink: TextEdit::new(""),
            job_seen: (0, 0),
            waker: None,
            loader: Latest::default(),
            pending: None,
            boot: None,
        };
        match listing {
            Some(first) => match first.result {
                Ok(v) => s.entries = v,
                Err(e) => s.note = Some(s.note.take().map_or(e.clone(), |n| format!("{e} · {n}"))),
            },
            // The folder is slow: open on an empty list showing "Loading…", like any navigation.
            None => {
                s.pending = Some(Pending { path: s.cwd.clone(), nav: Nav::Reload, select: Vec::new(), rename: false });
                s.boot = Some(boot);
            }
        }
        s.refilter(None);
        s
    }

    pub fn setup(&mut self, waker: rune::Waker) {
        self.loader.set_waker(waker.clone());
        if let Some(b) = &self.boot
            && let Ok(mut w) = b.waker.lock()
        {
            *w = Some(waker.clone());
        }
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

    /// The folder being loaded, if any.
    pub fn pending_path(&self) -> Option<&Path> {
        self.pending.as_ref().map(|p| p.path.as_path())
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
        let mut changed = self.poll_job();
        if let Some(listing) = self.poll_boot() {
            changed = true;
            self.land(listing, tx);
        }
        if let Some(listing) = self.loader.take() {
            changed |= self.land(listing, tx);
        }
        changed
    }

    /// What the startup read sent since the window opened: the settings are applied here, the
    /// first listing is returned to land like any other.
    fn poll_boot(&mut self) -> Option<Listing> {
        let mut listing = None;
        while let Some(b) = &self.boot {
            match b.rx.try_recv() {
                Ok(Boot::Start(s)) => self.land_start(*s),
                Ok(Boot::Listing(l)) => {
                    listing = Some(l);
                    self.boot = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.boot = None,
            }
        }
        listing
    }

    /// The settings, arriving after the window opened (a slow disk under the config folder).
    fn land_start(&mut self, start: Start) {
        let Start { config, start, places, note } = start;
        // Still waiting on the provisional folder: wait on the real start folder instead.
        if let Some(p) = self.pending.as_mut().filter(|p| p.nav == Nav::Reload && p.path == self.cwd) {
            p.path = start.clone();
            self.cwd = start;
        }
        self.sort = config.settings.sort;
        self.show_hidden = config.settings.show_hidden;
        self.places = places;
        self.note = config.summary().or(note).or(self.note.take());
        self.keymap = config.keymap;
        self.settings = config.settings;
        let keep = self.cursor_name();
        self.refilter(keep.as_deref());
    }

    /// Shows a finished listing if it's the one being waited for (a newer request wins).
    fn land(&mut self, listing: Listing, tx: &mut Tx) -> bool {
        let Some(p) = self.pending.take_if(|p| p.path == listing.path) else { return false };
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
        let by_prefix =
            || (!q.is_empty()).then(|| self.visible.iter().position(|&i| self.entries[i as usize].lower.starts_with(&q))).flatten();
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

    // ---- keys and commands -----------------------------------------------------------------

    pub fn on_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        tx.instant();
        // While renaming, the field has the keys; only Escape (which it passes on) gets here.
        if self.rename.is_some() {
            if ev.key == Key::Escape {
                self.rename = None;
                tx.blur();
            }
            return true;
        }
        if self.palette.is_some() {
            return self.palette_key(ev, tx);
        }
        if self.edit_filter(ev, tx) {
            return true;
        }
        if let Some(cmd) = self.keymap.find(|c| c.matches_tx(ev, tx)) {
            // A command that can't run lets the key go on (Esc with nothing to clear).
            if self.unavailable(cmd).is_some() {
                return false;
            }
            self.run(cmd, tx);
            return true;
        }
        match &ev.key {
            // Space with nothing typed is free (reserved for preview later).
            Key::Text(t) if t == " " && self.filter.is_empty() => false,
            Key::Text(t) if !ev.mods.cmd && !ev.mods.ctrl => {
                self.type_filter(t);
                self.reveal(tx);
                true
            }
            _ => false,
        }
    }

    /// While the filter has text, typing, ⌫ and ⌥⌫ edit it, and in go-to mode Enter goes,
    /// before any command sees those keys.
    fn edit_filter(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        if self.filter.is_empty() {
            return false;
        }
        let m = ev.mods;
        match &ev.key {
            Key::Backspace if m.alt && !m.cmd && !m.ctrl => {
                self.filter.clear();
                self.refilter(None);
            }
            Key::Backspace if !m.cmd && !m.ctrl && !m.alt => {
                self.filter.pop();
                let keep = self.cursor_name();
                self.refilter(keep.as_deref());
                self.reveal(tx);
            }
            Key::Enter if !m.cmd && !m.ctrl && !m.alt && self.path_mode() => self.submit_path(),
            Key::Text(t) if !m.cmd && !m.ctrl => {
                self.type_filter(t);
                self.reveal(tx);
            }
            _ => return false,
        }
        true
    }

    /// Why `cmd` can't run right now, or `None` when it can. Keys for an unavailable command
    /// pass through; the palette shows it dimmed and says this when it's chosen. Every command
    /// is listed, so a new one has to decide.
    pub fn unavailable(&self, cmd: Cmd) -> Option<&'static str> {
        use Cmd::*;
        let nothing = self.selected().is_none();
        let empty = self.visible.is_empty();
        // File operations run one at a time (`State::start`).
        let busy = self.job.is_some();
        let when = |cond: bool, why: &'static str| cond.then_some(why);
        match cmd {
            // Start a file operation.
            Duplicate | Trash => when(busy, BUSY).or(when(nothing, "Select an item first")),
            // Paste always shows as available otherwise: Rune can't ask the clipboard what it
            // holds without reading it (RUNE-GAPS.md). Pasting nothing says so.
            Paste | PasteMove => when(busy, BUSY),
            Undo => when(busy, BUSY).or(when(self.undo.is_empty(), "Nothing to undo")),
            Cancel => when(!busy, "No file operation is running"),
            Open | Copy | Cut | Rename => when(nothing, "Select an item first"),
            Into => when(!self.selected().is_some_and(|e| e.is_dir), "Select a folder first"),
            Parent => when(self.cwd.parent().is_none(), "Already at the top"),
            Back => when(self.back.is_empty(), "Nothing to go back to"),
            Forward => when(self.fwd.is_empty(), "Nothing to go forward to"),
            Clear => when(!busy && self.filter.is_empty() && self.note.is_none() && self.marked.is_empty(), "Nothing to clear"),
            Up | Down | PageUp | PageDown | First | Last | ExtendUp | ExtendDown | ExtendPageUp | ExtendPageDown | ExtendFirst
            | ExtendLast | SelectAll => when(empty, "Nothing to select"),
            SortName => when(self.sort.key == SortKey::Name, "Already sorted by name"),
            SortKind => when(self.sort.key == SortKey::Kind, "Already sorted by kind"),
            SortSize => when(self.sort.key == SortKey::Size, "Already sorted by size"),
            SortModified => when(self.sort.key == SortKey::Modified, "Already sorted by date modified"),
            Place1 | Place2 | Place3 | Place4 | Place5 | Place6 | Place7 | Place8 | Place9 => {
                when(cmd.place().is_some_and(|i| i >= self.places.len()), "No sidebar place there")
            }
            // Always available.
            GoTo | Reload | Reveal | Palette | OpenSettings | ReloadSettings | CopyPath | NewFolder | Hidden | SortReverse => None,
        }
    }

    /// The title the palette shows, with what's here filled in.
    pub fn title(&self, cmd: Cmd) -> Cow<'static, str> {
        match cmd {
            Cmd::Hidden if self.show_hidden => "Hide Hidden Files".into(),
            Cmd::Hidden => "Show Hidden Files".into(),
            _ => match cmd.place().and_then(|i| self.places.get(i)) {
                Some((name, _)) => format!("Go to {name}").into(),
                None => cmd.title().into(),
            },
        }
    }

    /// Runs `cmd`, from a key, the palette or the context menu. Check `unavailable` first.
    pub fn run(&mut self, cmd: Cmd, tx: &mut Tx) {
        use Cmd::*;
        let page = PAGE as isize;
        match cmd {
            Open => self.open_selected(),
            Into => {
                self.select(self.sel);
                self.open_selected();
            }
            Parent => self.parent(),
            Back => self.go_back(),
            Forward => self.go_forward(),
            GoTo => {
                let mut p = fs::pretty(&self.cwd);
                if !p.ends_with(std::path::MAIN_SEPARATOR) {
                    p.push(std::path::MAIN_SEPARATOR);
                }
                self.filter = p;
                self.refilter(None);
            }
            Reload => self.reload(),
            Reveal => self.reveal_selected(),
            Palette => self.open_palette(),
            OpenSettings => self.open_settings(),
            ReloadSettings => self.reload_settings(tx),
            Clear => {
                if let Some(job) = &self.job {
                    job.cancel();
                    self.note = Some("Cancelling…".into());
                } else {
                    self.note = None;
                    self.marked.clear();
                    let keep = self.cursor_name();
                    self.filter.clear();
                    self.refilter(keep.as_deref());
                    self.reveal(tx);
                }
            }
            Copy => self.copy_selected(false, tx),
            Cut => self.copy_selected(true, tx),
            // The paste lands in the root's paste handler (`view::view`).
            Paste | PasteMove => {
                self.paste_move = cmd == PasteMove;
                tx.request_paste();
            }
            CopyPath => self.copy_path(tx),
            Duplicate => self.duplicate(),
            Rename => self.start_rename(),
            NewFolder => self.new_folder(),
            Trash => self.trash_selected(),
            Undo => self.undo_last(),
            Cancel => {
                if let Some(job) = &self.job {
                    job.cancel();
                    self.note = Some("Cancelling…".into());
                }
            }
            Up => self.step(-1, false, tx),
            Down => self.step(1, false, tx),
            PageUp => self.step(-page, false, tx),
            PageDown => self.step(page, false, tx),
            First => self.step(isize::MIN / 2, false, tx),
            Last => self.step(isize::MAX / 2, false, tx),
            ExtendUp => self.step(-1, true, tx),
            ExtendDown => self.step(1, true, tx),
            ExtendPageUp => self.step(-page, true, tx),
            ExtendPageDown => self.step(page, true, tx),
            ExtendFirst => self.step(isize::MIN / 2, true, tx),
            ExtendLast => self.step(isize::MAX / 2, true, tx),
            SelectAll => self.select_all(),
            Hidden => self.toggle_hidden(),
            SortName => self.sort_by(SortKey::Name, tx),
            SortKind => self.sort_by(SortKey::Kind, tx),
            SortSize => self.sort_by(SortKey::Size, tx),
            SortModified => self.sort_by(SortKey::Modified, tx),
            SortReverse => {
                self.set_sort(self.sort.key);
                self.reveal(tx);
            }
            Place1 | Place2 | Place3 | Place4 | Place5 | Place6 | Place7 | Place8 | Place9 => {
                if let Some((_, p)) = cmd.place().and_then(|i| self.places.get(i)).cloned() {
                    self.go_to(p);
                }
            }
        }
    }

    fn sort_by(&mut self, key: SortKey, tx: &mut Tx) {
        self.set_sort(key);
        self.reveal(tx);
    }

    // ---- settings ---------------------------------------------------------------------------

    /// Creates the settings file from the template if there's none, then opens it in the
    /// default app.
    pub fn open_settings(&mut self) {
        self.note = Some(match config::ensure_file() {
            Err(e) => format!("Settings: {e}"),
            Ok(p) => match fs::open_text(&p) {
                Ok(()) => format!("Opened {} · run Reload Settings after saving", fs::pretty(&p)),
                Err(e) => format!("Settings are in {} (couldn't open it: {e})", fs::pretty(&p)),
            },
        });
    }

    /// Reads the settings file again. Keys apply at once; a preference applies only if the file
    /// changed it, so reloading doesn't undo ⌘. or a header click made since.
    pub fn reload_settings(&mut self, tx: &mut Tx) {
        let c = Config::load();
        let (old, new) = (&self.settings, &c.settings);
        let keep = self.cursor_name();
        if new.sort != old.sort {
            self.sort = new.sort;
            fs::sort_entries(&mut self.entries, self.sort);
        }
        if new.show_hidden != old.show_hidden {
            self.show_hidden = new.show_hidden;
        }
        if new.places != old.places {
            self.places = places_for(new);
        }
        self.refilter(keep.as_deref());
        self.reveal(tx);
        self.note = Some(c.summary().unwrap_or_else(|| "Settings reloaded".into()));
        self.keymap = c.keymap;
        self.settings = c.settings;
    }
}

/// How long the first window waits for the settings and first folder before opening anyway:
/// one frame at 120 Hz. A local folder is read long before this matters (the read starts at the
/// top of `main`, and the platform's launch takes 60–80 ms before `State::new` on macOS), so it
/// still makes the first frame; a slow or network folder costs the window at most this much,
/// then streams in like any navigation.
pub const FIRST_PAINT_WAIT: std::time::Duration = std::time::Duration::from_millis(8);

/// Headless runs (scripts, snapshots) run on a virtual clock and must be deterministic, so
/// they wait for the first listing (bounded, in case the folder never answers).
const HEADLESS_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// The settings and the folder to open, from the startup read.
struct Start {
    config: Config,
    start: PathBuf,
    places: Vec<(String, PathBuf)>,
    /// A problem with the start folder.
    note: Option<String>,
}

/// What the startup read sends, in this order.
enum Boot {
    Start(Box<Start>),
    Listing(Listing),
}

/// The startup read: a thread reading the settings, then the first folder, sending each as it
/// finishes and waking the window once it has one.
struct Booting {
    rx: std::sync::mpsc::Receiver<Boot>,
    waker: std::sync::Arc<std::sync::Mutex<Option<rune::Waker>>>,
}

impl Booting {
    fn start() -> Booting {
        let slow = std::time::Duration::from_millis(SLOW_FIRST_READ_MS.load(std::sync::atomic::Ordering::Relaxed));
        Booting::spawn(slow, || {
            let config = Config::load();
            let (start, note) = start_dir(&config.settings);
            let places = places_for(&config.settings);
            Start { config, start, places, note }
        })
    }

    /// Runs `settings` then lists the folder it names (`slow` later), on a new thread.
    fn spawn(slow: std::time::Duration, settings: impl FnOnce() -> Start + Send + 'static) -> Booting {
        let (tx, rx) = std::sync::mpsc::channel();
        let waker: std::sync::Arc<std::sync::Mutex<Option<rune::Waker>>> = Default::default();
        let wake = waker.clone();
        let send = move |b: Boot| {
            // The window may be gone (or not yet open, when `setup` hasn't run: then `poll` on
            // its first frame picks this up).
            if tx.send(b).is_ok()
                && let Some(w) = wake.lock().ok().and_then(|w| w.clone())
            {
                w();
            }
        };
        std::thread::spawn(move || {
            let start = settings();
            let (dir, sort) = (start.start.clone(), start.config.settings.sort);
            send(Boot::Start(Box::new(start)));
            if !slow.is_zero() {
                std::thread::sleep(slow);
            }
            send(Boot::Listing(fs::list(dir, sort)));
        });
        Booting { rx, waker }
    }
}

/// `ARBOR_SLOW_FIRST_READ_MS`, read by `timing::init` only when timing is on (otherwise 0 and
/// never set): makes the first folder read that much slower, to check that a slow disk doesn't
/// hold up the window.
pub static SLOW_FIRST_READ_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The sidebar: the settings file's places, or the ones found on this machine.
fn places_for(s: &Settings) -> Vec<(String, PathBuf)> {
    let Some(list) = &s.places else { return fs::places() };
    let home = fs::home();
    list.iter()
        .map(|p| {
            let name = if *p == home {
                "Home".to_string()
            } else {
                p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
            };
            (name, p.clone())
        })
        .collect()
}

/// The startup read, started by `main` before Rune's event loop so it overlaps the platform's
/// app launch instead of following it.
static BOOT: std::sync::Mutex<Option<Booting>> = std::sync::Mutex::new(None);

/// Starts reading the settings file and the folder Arbor opens in. Call first thing in `main`.
pub fn read_first_folder() {
    let boot = Booting::start();
    if let Ok(mut slot) = BOOT.lock() {
        *slot = Some(boot);
    }
}

/// The folder named on the command line, else the settings' start folder, else the current
/// one (or home when launched from `/`). Also a note when the start folder isn't there.
fn start_dir(s: &Settings) -> (PathBuf, Option<String>) {
    if let Some(p) = std::env::args_os().skip(1).map(PathBuf::from).find(|p| p.is_dir()) {
        return (p, None);
    }
    let mut note = None;
    if let Some(p) = &s.start_folder {
        if p.is_dir() {
            return (p.clone(), None);
        }
        note = Some(format!("Settings: start_folder {} isn't a folder", fs::pretty(p)));
    }
    let dir = std::env::current_dir().ok().filter(|p| p != Path::new("/")).unwrap_or_else(fs::home);
    (dir, note)
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
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    rune::LocalTime::at(ms)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn folder(name: &str, files: usize) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for i in 0..files {
            std::fs::write(d.join(format!("f{i}")), "x").unwrap();
        }
        d
    }

    fn settings_for(dir: PathBuf, settings: Settings) -> Start {
        Start { config: Config { settings, ..Config::default() }, start: dir, places: Vec::new(), note: None }
    }

    /// Polls as frames would until nothing is loading (or 10 s pass).
    fn settle(s: &mut State) {
        let until = Instant::now() + Duration::from_secs(10);
        while (s.is_loading() || s.boot.is_some()) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
            s.poll(&mut Tx::default());
        }
        assert!(!s.is_loading(), "never landed");
    }

    #[test]
    fn a_slow_first_folder_doesnt_hold_up_the_window() {
        let d = folder("slow", 3);
        let dir = d.clone();
        let boot = Booting::spawn(Duration::from_millis(400), move || settings_for(dir, Settings::default()));
        let t = Instant::now();
        let mut s = State::boot(boot, FIRST_PAINT_WAIT);
        // The bounded wait, plus scheduling slack for a loaded CI machine: far under the 400 ms read.
        assert!(t.elapsed() < Duration::from_millis(150), "waited {:?}", t.elapsed());
        assert!(s.is_loading() && s.entries.is_empty() && s.cwd == d);
        settle(&mut s);
        assert_eq!((s.entries.len(), s.cwd.clone()), (3, d));
    }

    #[test]
    fn a_fast_first_folder_makes_the_first_frame() {
        let d = folder("fast", 3);
        let dir = d.clone();
        let boot = Booting::spawn(Duration::ZERO, move || settings_for(dir, Settings::default()));
        // Returns as soon as both are in, not at the end of the wait.
        let t = Instant::now();
        let s = State::boot(boot, Duration::from_secs(5));
        assert!(t.elapsed() < Duration::from_secs(1));
        assert!(!s.is_loading() && s.boot.is_none());
        assert_eq!((s.entries.len(), s.visible.len()), (3, 3));
    }

    #[test]
    fn late_settings_apply_when_they_land() {
        let d = folder("late-settings", 3);
        std::fs::write(d.join(".hidden"), "x").unwrap();
        let dir = d.clone();
        let boot = Booting::spawn(Duration::ZERO, move || {
            std::thread::sleep(Duration::from_millis(300));
            settings_for(dir, Settings { show_hidden: true, ..Settings::default() })
        });
        let mut s = State::boot(boot, FIRST_PAINT_WAIT);
        assert!(s.is_loading() && !s.show_hidden, "opens with the defaults");
        settle(&mut s);
        assert!(s.show_hidden && s.cwd == d);
        assert_eq!(s.visible.len(), 4);
        assert!(s.back.is_empty(), "the first folder isn't a navigation");
    }

    pub(crate) fn state_in(d: &Path) -> State {
        let dir = d.to_path_buf();
        State::boot(Booting::spawn(Duration::ZERO, move || settings_for(dir, Settings::default())), Duration::from_secs(5))
    }

    #[test]
    fn file_operations_are_unavailable_while_one_runs() {
        let d = folder("busy", 2);
        let mut s = state_in(&d);
        assert_eq!(s.unavailable(Cmd::Duplicate), None);
        assert_eq!(s.unavailable(Cmd::Cancel), Some("No file operation is running"));
        s.job = Some(ops::start(ops::Op::Copy { sources: Vec::new(), dest: d.clone() }, None));
        for cmd in [Cmd::Duplicate, Cmd::Trash, Cmd::Paste, Cmd::PasteMove, Cmd::Undo] {
            assert_eq!(s.unavailable(cmd), Some(BUSY), "{cmd:?}");
        }
        for cmd in [Cmd::Cancel, Cmd::Clear, Cmd::Copy, Cmd::Rename, Cmd::Down] {
            assert_eq!(s.unavailable(cmd), None, "{cmd:?}");
        }
    }

    /// Going somewhere before the first listing lands: the newer request wins.
    #[test]
    fn a_navigation_overtakes_a_slow_first_folder() {
        let (slow, other) = (folder("overtaken", 3), folder("overtaker", 2));
        let dir = slow.clone();
        let boot = Booting::spawn(Duration::from_millis(300), move || settings_for(dir, Settings::default()));
        let mut s = State::boot(boot, FIRST_PAINT_WAIT);
        s.go(other.clone(), Nav::New, None);
        settle(&mut s);
        std::thread::sleep(Duration::from_millis(400)); // the slow one lands too, and is dropped
        s.poll(&mut Tx::default());
        assert_eq!((s.entries.len(), s.cwd.clone()), (2, other));
    }
}
