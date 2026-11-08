//! App state and everything that changes it: navigation, selection, filtering, keys.
//! File operations live in `actions.rs`, tabs and panes in `panes.rs`, and one folder's
//! listing and selection in `folder.rs`. Every handler lands its change with `tx.instant()`:
//! a file manager should never animate.

use std::borrow::Cow;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use rune::{Key, KeyEvent, Latest, TextEdit, Tx};

use crate::commands::{Cmd, Keymap};
use crate::config::{self, Config, Settings};
pub use crate::folder::Nav;
use crate::folder::{Answer, Folder, TabMemo};
use crate::fs::{self, Entry, Listing, Sort, SortKey};
use crate::jump::{History, JumpBox};
use crate::ops;
use crate::palette::Palette;
use crate::preview::{self, Preview};
use crate::select::PickBox;

/// Rows a Page Up / Page Down moves.
const PAGE: usize = 20;

/// Why a command that starts a file operation can't run while another one does.
pub(crate) const BUSY: &str = "Another file operation is running";

/// An inline rename in progress (always in the active pane).
pub struct Rename {
    pub os: OsString,
    pub edit: TextEdit,
}

/// What ⌘C / ⌘X put aside, so a paste knows a cut means move.
pub(crate) struct Clip {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
}

/// ⌘V or ⌥⌘V, waiting for the clipboard: the folder that was on screen in the active pane
/// when it was asked for (its path and its identity, from its listing), and whether it moves.
/// Each request has its own, answered by its own clipboard read (`id`), so two pastes asked
/// before either answer each go where, and as, they were asked.
pub(crate) struct PasteReq {
    pub id: u64,
    pub dest: PathBuf,
    pub dest_id: Option<crate::noclobber::FileId>,
    pub mv: bool,
}

/// One side of the window: its tabs, and the visible tab's live folder.
pub struct Pane {
    /// The visible tab's folder (its `id` is the pane's).
    pub f: Folder,
    /// Every tab, in order. The visible one is `None`: its state is `f`, live.
    pub tabs: Vec<Option<TabMemo>>,
    /// Which tab is visible.
    pub tab: usize,
}

impl Pane {
    pub fn id(&self) -> u64 {
        self.f.id
    }

    /// Tab `i`'s folder.
    pub fn tab_path(&self, i: usize) -> &Path {
        match self.tabs.get(i) {
            Some(Some(m)) => &m.path,
            _ => &self.f.cwd,
        }
    }
}

pub struct State {
    /// One pane, or two side by side (left first).
    pub panes: Vec<Pane>,
    /// The pane that gets keys, commands and the status bar.
    pub active: usize,
    /// Closed tabs, newest last, for "Reopen Closed Tab". Lightweight state only.
    pub(crate) closed: Vec<TabMemo>,
    next_pane: u64,
    /// How long showing a hidden tab waits for its fresh read before drawing without it.
    pub(crate) show_wait: std::time::Duration,
    /// Counts listings landed, for timing.
    pub lands: u64,
    /// The pane a row drag between panes is over.
    pub pane_drop: Option<u64>,
    pub show_hidden: bool,
    /// The user toggled hidden files (even back to where it was), so settings that land late
    /// at startup leave it alone.
    hidden_set: bool,
    pub places: Vec<(String, PathBuf)>,
    /// The last error or note, shown in the status bar until the next action.
    pub note: Option<String>,
    pub rename: Option<Rename>,
    /// The running file operation, if any.
    pub job: Option<ops::Running>,
    pub(crate) clip: Option<Clip>,
    /// Pastes asked for whose clipboard reads haven't come back yet, oldest first.
    pub(crate) paste_reqs: Vec<PasteReq>,
    next_paste: u64,
    pub(crate) undo: Vec<ops::Undo>,
    /// Which keys run which command (defaults plus the settings file).
    pub keymap: Keymap,
    /// The settings as last read, so "Reload Settings" applies only what changed.
    pub(crate) settings: Settings,
    pub palette: Option<Palette>,
    /// Jump to Folder's box, while it's open (`jump.rs`).
    pub jump: Option<JumpBox>,
    /// Select by Pattern's box, while it's open (`select.rs`).
    pub pick: Option<PickBox>,
    /// The folders visited, for Jump: nothing is read until it first opens.
    pub history: History,
    /// The read a Jump asked for, so a folder that's gone is dropped from the history.
    pub(crate) jump_req: Option<u64>,
    /// The confirmation dialog, while it asks about a command that can't be undone
    /// (`confirm.rs`).
    pub asking: Option<crate::confirm::Ask>,
    /// The palette's, Jump's and Select by Pattern's late field writes after they close (see
    /// `rename_sink`).
    pub(crate) palette_sink: TextEdit,
    /// Where the rename field's late writes go after the rename has ended (the runtime
    /// clears the field's focus through its binding a frame after a commit).
    rename_sink: TextEdit,
    pub(crate) job_seen: (u64, usize),
    pub(crate) waker: Option<rune::Waker>,
    /// "Reload Settings", reading the file on a worker.
    settings_loader: Latest<Config>,
    /// The startup read, while the settings or first listing haven't landed yet.
    boot: Option<Booting>,
    /// The first folder's read request, while it's the one its pane waits for: the startup
    /// listing lands only for it, and late settings retarget only it.
    boot_req: Option<u64>,
    /// Quick preview, while it's open (`preview.rs`). Closed, it costs nothing.
    pub preview: Option<Preview>,
    /// The preview's worker thread, from the first preview on (kept across opens).
    pub(crate) preview_worker: Option<preview::Worker>,
    /// The preview panel's laid-out size in device pixels, written by the view.
    pub preview_fit: preview::Fit,
    /// Preview reads started and answers landed, for tests and timing.
    pub preview_reads: u64,
    pub preview_lands: u64,
    /// The threads searches walk on, from the first search on (kept, idle between searches).
    pub(crate) search_pool: Option<crate::search::Pool>,
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
                Ok(Boot::Listing(_, l)) => listing = Some(l),
                Err(_) => break,
            }
        }
        // Without the settings yet: the defaults, in the folder Arbor was started in, until
        // they land (`land_start`).
        let (config, cwd, places, note) = match start {
            Some(s) => (s.config, s.start, s.places, s.note),
            None => (Config::default(), std::env::current_dir().unwrap_or_else(|_| fs::home()), Vec::new(), None),
        };
        let first = Folder::new(0, cwd, config.settings.sort, config.settings.show_hidden, None);
        // Where the history lives (environment only, no disk): tests keep theirs in memory.
        let mut history = History::new(if cfg!(test) { None } else { crate::jump::log_path() });
        history.enabled = config.settings.jump_enabled;
        history.exclude = crate::jump::exclude_globs(&config.settings.jump_exclude);
        let mut s = State {
            panes: vec![Pane { f: first, tabs: vec![None], tab: 0 }],
            active: 0,
            closed: Vec::new(),
            next_pane: 1,
            show_wait: FIRST_PAINT_WAIT,
            lands: 0,
            pane_drop: None,
            show_hidden: config.settings.show_hidden,
            hidden_set: false,
            places,
            note: config.summary().or(note),
            rename: None,
            job: None,
            clip: None,
            paste_reqs: Vec::new(),
            next_paste: 0,
            undo: Vec::new(),
            keymap: config.keymap,
            settings: config.settings,
            palette: None,
            jump: None,
            pick: None,
            history,
            jump_req: None,
            asking: None,
            palette_sink: TextEdit::new(""),
            rename_sink: TextEdit::new(""),
            job_seen: (0, 0),
            waker: None,
            settings_loader: Latest::default(),
            boot: None,
            boot_req: None,
            preview: None,
            preview_worker: None,
            preview_fit: std::rc::Rc::new(std::cell::Cell::new(preview::DEFAULT_FIT)),
            preview_reads: 0,
            preview_lands: 0,
            search_pool: None,
        };
        let f = &mut s.panes[0].f;
        match listing {
            Some(first) => match first.result {
                Ok(v) => {
                    f.entries = v;
                    f.dir_id = first.dir_id;
                }
                Err(e) => s.note = Some(s.note.take().map_or(e.clone(), |n| format!("{e} · {n}"))),
            },
            // The folder is slow: open on an empty list showing "Loading…", like any navigation.
            None => {
                let req = crate::folder::next_request();
                f.pending = Some(crate::folder::Pending {
                    req,
                    path: f.cwd.clone(),
                    nav: Nav::Reload,
                    select: Vec::new(),
                    rename: false,
                    scroll: 0.0,
                });
                s.boot = Some(boot);
                s.boot_req = Some(req);
            }
        }
        s.panes[0].f.refilter(None);
        s
    }

    pub fn setup(&mut self, waker: rune::Waker) {
        for p in &self.panes {
            p.f.loader.set_waker(waker.clone());
        }
        self.settings_loader.set_waker(waker.clone());
        if let Some(b) = &self.boot
            && let Ok(mut w) = b.waker.lock()
        {
            *w = Some(waker.clone());
        }
        self.waker = Some(waker);
    }

    /// The active pane's folder.
    pub fn cur(&self) -> &Folder {
        &self.panes[self.active].f
    }

    pub fn cur_mut(&mut self) -> &mut Folder {
        &mut self.panes[self.active].f
    }

    pub fn split(&self) -> bool {
        self.panes.len() > 1
    }

    /// The index of the pane with this id (a handler from an older frame may name a pane that
    /// has closed since).
    pub fn pane_ix(&self, id: u64) -> Option<usize> {
        self.panes.iter().position(|p| p.id() == id)
    }

    pub fn pane(&self, id: u64) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id() == id)
    }

    pub(crate) fn new_pane_id(&mut self) -> u64 {
        self.next_pane += 1;
        self.next_pane - 1
    }

    pub fn rename_edit(&mut self) -> &mut TextEdit {
        match &mut self.rename {
            Some(r) => &mut r.edit,
            None => &mut self.rename_sink,
        }
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.cur().selected()
    }

    pub fn target_paths(&self) -> Vec<PathBuf> {
        self.cur().target_paths()
    }

    pub fn is_loading(&self) -> bool {
        self.cur().is_loading()
    }

    /// The folder the active pane is loading, if any.
    pub fn pending_path(&self) -> Option<&Path> {
        self.cur().pending.as_ref().map(|p| p.path.as_path())
    }

    // ---- navigation -------------------------------------------------------------------------

    pub(crate) fn go_full(&mut self, path: PathBuf, nav: Nav, select: Vec<OsString>, rename: bool) {
        self.rename = None;
        self.cur_mut().request(path, nav, select, rename);
    }

    pub fn go(&mut self, path: PathBuf, nav: Nav, select: Option<OsString>) {
        self.go_full(path, nav, select.into_iter().collect(), false);
    }

    pub fn go_to(&mut self, path: PathBuf) {
        if path != self.cur().cwd {
            self.go(path, Nav::New, None);
        }
    }

    pub fn parent(&mut self) {
        let f = self.cur();
        if let Some(p) = f.cwd.parent() {
            let came_from = f.cwd.file_name().map(|n| n.to_os_string());
            self.go(p.to_path_buf(), Nav::New, came_from);
        }
    }

    pub fn go_back(&mut self) {
        let f = self.cur();
        if let Some(p) = f.back.last().cloned() {
            let came_from = f.cwd.file_name().map(|n| n.to_os_string());
            let select = if f.cwd.parent() == Some(p.as_path()) { came_from } else { None };
            self.go(p, Nav::Back, select);
        }
    }

    pub fn go_forward(&mut self) {
        if let Some(p) = self.cur().fwd.last().cloned() {
            self.go(p, Nav::Forward, None);
        }
    }

    /// Re-reads the active folder, keeping the selection (or selecting `names` instead).
    pub fn reload_selecting(&mut self, names: Vec<OsString>, rename: bool) {
        self.rename = None;
        reload_folder(self.cur_mut(), names, rename);
    }

    /// ⌘R: reads the folder again, or in a search walks again.
    pub fn reload(&mut self) {
        if self.cur().searching() {
            let ix = self.active;
            self.restart_search(ix, &mut Tx::default());
            return;
        }
        self.reload_selecting(Vec::new(), false);
    }

    /// Opens the cursor's entry: a folder navigates, a file opens in its default app.
    /// With several files selected, opens each file.
    pub fn open_selected(&mut self) {
        let f = self.cur();
        let targets: Vec<(PathBuf, bool)> = f.targets().iter().map(|e| (f.cwd.join(&e.os), e.is_dir)).collect();
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
        let f = self.cur();
        let path = f.selected().map_or_else(|| f.cwd.clone(), |e| f.cwd.join(&e.os));
        if let Err(err) = fs::reveal(&path) {
            self.note = Some(format!("Couldn't reveal: {err}"));
        }
    }

    /// Called from `poll`: lands a finished listing or file operation. True when the view
    /// needs to change.
    pub fn poll(&mut self, tx: &mut Tx) -> bool {
        tx.instant();
        let mut changed = self.poll_job();
        changed |= self.poll_boot(tx);
        for ix in 0..self.panes.len() {
            if let Some(a) = self.panes[ix].f.loader.take() {
                changed |= self.land(ix, a, tx);
            }
        }
        if let Some(c) = self.settings_loader.take() {
            self.apply_settings(c, tx);
            changed = true;
        }
        changed |= self.poll_history();
        changed |= self.poll_search();
        // After everything that can move the selection this frame.
        changed |= self.sync_preview();
        changed |= self.land_preview();
        changed
    }

    /// What the startup read sent since the window opened: the settings, then the first
    /// listing, which lands like any other but only for the request it answers. True when
    /// anything changed.
    fn poll_boot(&mut self, tx: &mut Tx) -> bool {
        let mut changed = false;
        while let Some(b) = &self.boot {
            match b.rx.try_recv() {
                Ok(Boot::Start(s)) => {
                    self.land_start(*s);
                    changed = true;
                }
                Ok(Boot::Listing(sort, listing)) => {
                    self.boot = None;
                    // Gone when the user navigated (or reloaded) first: then it's stale.
                    let req = self.boot_req.take();
                    let ix = self.panes.iter().position(|p| req.is_some() && p.f.pending.as_ref().map(|p| p.req) == req);
                    if let (Some(ix), Some(req)) = (ix, req) {
                        changed |= self.land(ix, Answer { req, sort, listing }, tx);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.boot = None,
            }
        }
        changed
    }

    /// The settings, arriving after the window opened (a slow disk under the config folder).
    /// A folder only ever changes together with its listing: the provisional first folder is
    /// retargeted to the start folder only while its first read is still the one it waits for
    /// (nothing shown, nobody navigated). A preference applies only where the user hasn't set
    /// it since the window opened on the defaults (even back to the default: a flag says so,
    /// not a comparison).
    fn land_start(&mut self, start: Start) {
        let Start { config, start, places, note } = start;
        if let Some(req) = self.boot_req
            && let Some(f) = self.panes.iter_mut().map(|p| &mut p.f).find(|f| f.pending.as_ref().is_some_and(|p| p.req == req))
            && f.entries.is_empty()
            && let Some(p) = f.pending.as_mut()
        {
            p.path = start.clone();
            f.cwd = start;
        }
        let (old, new) = (&self.settings, &config.settings);
        if !self.hidden_set {
            self.show_hidden = new.show_hidden;
            for p in &mut self.panes {
                p.f.set_hidden(new.show_hidden);
            }
        }
        for p in &mut self.panes {
            if !p.f.sort_set && new.sort != old.sort {
                p.f.apply_sort(new.sort);
            }
            for m in p.tabs.iter_mut().flatten().filter(|m| !m.sort_set) {
                m.sort = new.sort;
            }
        }
        self.places = places;
        self.apply_history_settings(&config.settings);
        self.note = config.summary().or(note).or(self.note.take());
        self.keymap = config.keymap;
        self.settings = config.settings;
    }

    /// Shows a finished listing in pane `ix` if it's the one being waited for (a newer request
    /// wins).
    pub(crate) fn land(&mut self, ix: usize, a: Answer, tx: &mut Tx) -> bool {
        let active = ix == self.active;
        let f = &mut self.panes[ix].f;
        let Some(p) = f.pending.take_if(|p| p.req == a.req && p.path == a.listing.path) else { return false };
        let jumped = self.jump_req.take_if(|&mut r| r == p.req).is_some();
        match a.listing.result {
            Err(_) if jumped && a.listing.missing => self.jump_missed(&p.path),
            Err(e) => {
                self.note = Some(format!("{}: {e}", fs::pretty(&p.path)));
            }
            Ok(mut entries) => {
                self.lands += 1;
                // The sort changed while it was being read.
                if a.sort != f.sort {
                    fs::sort_entries(&mut entries, f.sort);
                }
                let old = std::mem::replace(&mut f.cwd, p.path);
                f.dir_id = a.listing.dir_id;
                let moved = old != f.cwd;
                match p.nav {
                    Nav::New => {
                        f.back.push(old);
                        f.fwd.clear();
                    }
                    Nav::Back => {
                        f.back.pop();
                        f.fwd.push(old);
                    }
                    Nav::Forward => {
                        f.fwd.pop();
                        f.back.push(old);
                    }
                    Nav::Reload | Nav::Show => {}
                }
                f.entries = entries;
                f.now = crate::folder::now();
                let navigated = matches!(p.nav, Nav::New | Nav::Back | Nav::Forward);
                if navigated {
                    f.filter.clear();
                    // For Jump: one message to the history's writer, no disk I/O here.
                    let here = f.cwd.clone();
                    self.history.record(&here);
                }
                let f = &mut self.panes[ix].f;
                f.marked.clear();
                if p.select.len() > 1 {
                    f.marked.extend(p.select.iter().cloned());
                }
                f.refilter(p.select.first().map(OsString::as_os_str));
                f.anchor = f.sel;
                if p.nav == Nav::Show {
                    // Where it was when it was hidden (clamped if the folder shrank since).
                    tx.scroll_to_instant(f.list_key(), 0.0, p.scroll);
                } else {
                    if moved {
                        tx.scroll_to_instant(f.list_key(), 0.0, 0.0);
                    }
                    f.reveal(tx);
                }
                if active {
                    if navigated {
                        self.note = None;
                    }
                    tx.set_title(title(&self.cur().cwd));
                    if p.rename {
                        self.start_rename();
                    }
                }
            }
        }
        self.sync_preview();
        true
    }

    // ---- selection, sorting, filtering ------------------------------------------------------

    pub fn set_sort(&mut self, key: SortKey) {
        self.cur_mut().set_sort(key);
    }

    pub fn toggle_hidden(&mut self) {
        self.hidden_set = true;
        self.show_hidden = !self.show_hidden;
        let on = self.show_hidden;
        for p in &mut self.panes {
            p.f.set_hidden(on);
        }
    }

    pub fn reveal(&self, tx: &mut Tx) {
        self.cur().reveal(tx);
    }

    fn submit_path(&mut self) {
        let path = fs::expand(self.cur().filter.trim());
        self.go_path(&path);
    }

    /// Go-to: a folder navigates, a file opens, anything else says it isn't there.
    pub(crate) fn go_path(&mut self, path: &Path) {
        let path = path.to_path_buf();
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
        let handled = self.key(ev, tx);
        if handled {
            self.sync_preview();
        }
        handled
    }

    fn key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        tx.instant();
        // The confirmation dialog is modal: it has every key.
        if self.asking.is_some() {
            return self.ask_key(ev, tx);
        }
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
        if self.jump.is_some() {
            return self.jump_key(ev, tx);
        }
        if self.pick.is_some() {
            return self.pick_key(ev, tx);
        }
        // In a search with the results focused, ⌫ edits the query (as typing does, below).
        let m = ev.mods;
        if self.cur().searching() && ev.key == Key::Backspace && !m.cmd && !m.ctrl && !m.alt {
            self.type_in_search(self.active, None, tx);
            return true;
        }
        if self.edit_filter(ev, tx) {
            return true;
        }
        if let Some(mut cmd) = self.keymap.find(|c| c.matches_tx(ev, tx)) {
            // Esc closes the preview before it clears anything (the palette and a rename have
            // had it first, above).
            if cmd == Cmd::Clear && self.preview.is_some() {
                cmd = Cmd::ClosePreview;
            }
            // Then a running file operation; then in a search, it stops the walk or leaves.
            if cmd == Cmd::Clear && self.job.is_none() && self.cur().searching() {
                self.search_escape(self.active, tx);
                return true;
            }
            if let Some(why) = self.unavailable(cmd) {
                // Closing the last tab does nothing, the same on every platform: passed on, ⌘W
                // would close the window on macOS (Rune's standard Close item).
                if cmd == Cmd::CloseTab {
                    self.note = Some(why.into());
                    return true;
                }
                // Any other command that can't run lets the key go on (Esc with nothing to clear).
                return false;
            }
            self.run(cmd, tx);
            return true;
        }
        if self.cur().searching() {
            if let Key::Text(t) = &ev.key
                && !ev.mods.cmd
                && !ev.mods.ctrl
            {
                self.type_in_search(self.active, Some(t), tx);
                return true;
            }
            return false;
        }
        let f = self.cur_mut();
        match &ev.key {
            // Space with nothing typed (and Toggle Preview unbound from it) types nothing: a
            // filter never starts with a space.
            Key::Text(t) if t == " " && f.filter.is_empty() => false,
            Key::Text(t) if !ev.mods.cmd && !ev.mods.ctrl => {
                f.type_filter(t);
                f.reveal(tx);
                true
            }
            _ => false,
        }
    }

    /// While the filter has text, typing, ⌫ and ⌥⌫ edit it, and in go-to mode Enter goes,
    /// before any command sees those keys. Tab in go-to mode is reserved (path completion).
    fn edit_filter(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        let f = self.cur_mut();
        if f.filter.is_empty() {
            return false;
        }
        let m = ev.mods;
        match &ev.key {
            Key::Backspace if m.alt && !m.cmd && !m.ctrl => {
                f.filter.clear();
                f.refilter(None);
            }
            Key::Backspace if !m.cmd && !m.ctrl && !m.alt => {
                f.filter.pop();
                let keep = f.cursor_name();
                f.refilter(keep.as_deref());
                f.reveal(tx);
            }
            Key::Tab if !m.cmd && !m.ctrl && !m.alt && f.path_mode() => {}
            Key::Enter if !m.cmd && !m.ctrl && !m.alt && f.path_mode() => self.submit_path(),
            Key::Text(t) if !m.cmd && !m.ctrl => {
                f.type_filter(t);
                f.reveal(tx);
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
        let f = self.cur();
        let nothing = f.selected().is_none();
        let empty = f.visible.is_empty();
        // File operations run one at a time (`State::start`).
        let busy = self.job.is_some();
        let pane = &self.panes[self.active];
        let when = |cond: bool, why: &'static str| cond.then_some(why);
        let searching = f.searching();
        match cmd {
            // Start a file operation.
            Trash => when(busy, BUSY).or(when(nothing, "Select an item first")),
            // A duplicate goes beside its original, so all of them must be in one folder.
            Duplicate => when(busy, BUSY).or(when(nothing, "Select an item first")).or(when(
                searching && f.target_paths().iter().map(|p| p.parent()).collect::<std::collections::HashSet<_>>().len() > 1,
                "Duplicate items from one folder at a time",
            )),
            CopyToOther | MoveToOther => when(!self.split(), NOT_SPLIT)
                .or(when(busy, BUSY))
                .or(when(nothing, "Select an item first"))
                .or(when(self.other().is_some_and(Folder::searching), IN_OTHER_SEARCH))
                .or(when(self.other().is_some_and(|o| o.shows(&f.cwd, f.dir_id)), "Both panes show the same folder")),
            // Paste always shows as available otherwise: Rune can't ask the clipboard what it
            // holds without reading it (RUNE-GAPS.md). Pasting nothing says so.
            Paste | PasteMove => when(busy, BUSY).or(when(searching, "Leave the search (Esc) to paste into this folder")),
            Undo => when(busy, BUSY).or(when(self.undo.is_empty(), "Nothing to undo")),
            Cancel => when(!busy, "No file operation is running"),
            Open | Copy | Cut | Rename => when(nothing, "Select an item first"),
            Into => when(!f.selected().is_some_and(|e| e.is_dir), "Select a folder first"),
            Parent => when(searching, "Leave the search first (Esc)").or(when(f.cwd.parent().is_none(), "Already at the top")),
            // In a search, Back leaves it.
            Back => when(!searching && f.back.is_empty(), "Nothing to go back to"),
            Forward => when(f.fwd.is_empty(), "Nothing to go forward to"),
            Clear => when(!busy && !searching && f.filter.is_empty() && self.note.is_none() && f.marked.is_empty(), "Nothing to clear"),
            Up | Down | PageUp | PageDown | First | Last | ExtendUp | ExtendDown | ExtendPageUp | ExtendPageDown | ExtendFirst
            | ExtendLast | SelectAll | SelectPattern | InvertSelection => when(empty, "Nothing to select"),
            SelectKind => when(nothing, "Select an item first"),
            Jump => when(!self.history.enabled, "Jump is turned off in the settings (jump.enabled)"),
            SortName => when(f.sort.key == SortKey::Name, "Already sorted by name"),
            SortKind => when(f.sort.key == SortKey::Kind, "Already sorted by kind"),
            SortSize => when(f.sort.key == SortKey::Size, "Already sorted by size"),
            SortModified => when(f.sort.key == SortKey::Modified, "Already sorted by date modified"),
            CloseTab => when(!self.split() && pane.tabs.len() == 1, "This is the only tab"),
            NextTab | PrevTab => when(pane.tabs.len() == 1, "There's only one tab here"),
            ReopenTab => when(self.closed.is_empty(), "No closed tabs to reopen"),
            OtherPane => when(!self.split(), NOT_SPLIT),
            // Closing always works; opening needs something to show.
            TogglePreview => when(self.preview.is_none() && nothing, "Select an item first"),
            ClosePreview => when(self.preview.is_none(), "The preview isn't open"),
            Place1 | Place2 | Place3 | Place4 | Place5 | Place6 | Place7 | Place8 | Place9 => {
                when(cmd.place().is_some_and(|i| i >= self.places.len()), "No sidebar place there")
            }
            // Always available.
            NewFolder => when(searching, "Leave the search (Esc) to make a folder here"),
            GoTo | Reload | Reveal | Palette | OpenSettings | ReloadSettings | CopyPath | Hidden | SortReverse | NewTab | Split
            | ClearJump | Search => None,
        }
    }

    /// The title the palette shows, with what's here filled in.
    pub fn title(&self, cmd: Cmd) -> Cow<'static, str> {
        match cmd {
            Cmd::Hidden if self.show_hidden => "Hide Hidden Files".into(),
            Cmd::Hidden => "Show Hidden Files".into(),
            Cmd::Split if self.split() => "Close Split View".into(),
            Cmd::Split => "Split View".into(),
            Cmd::TogglePreview if self.preview.is_some() => "Close Preview".into(),
            Cmd::TogglePreview => "Quick Preview".into(),
            _ => match cmd.place().and_then(|i| self.places.get(i)) {
                Some((name, _)) => format!("Go to {name}").into(),
                None => cmd.title().into(),
            },
        }
    }

    /// Runs `cmd`, from a key, the palette or the context menu. Check `unavailable` first.
    pub fn run(&mut self, cmd: Cmd, tx: &mut Tx) {
        // What can't be undone asks first; only the dialog's button runs it (`run_permanent`).
        if cmd.permanent() {
            self.ask(cmd, tx);
            return;
        }
        self.run_cmd(cmd, tx);
        self.sync_preview();
    }

    /// Runs a permanent command, confirmed in the dialog.
    pub(crate) fn run_permanent(&mut self, cmd: Cmd, tx: &mut Tx) {
        self.run_cmd(cmd, tx);
        self.sync_preview();
    }

    fn run_cmd(&mut self, cmd: Cmd, tx: &mut Tx) {
        use Cmd::*;
        let page = PAGE as isize;
        match cmd {
            Open => self.open_selected(),
            Into => {
                let f = self.cur_mut();
                f.select(f.sel);
                self.open_selected();
            }
            Parent => self.parent(),
            Back if self.cur().searching() => self.leave_search(self.active, tx),
            Back => self.go_back(),
            Forward => self.go_forward(),
            GoTo => {
                self.leave_search(self.active, tx);
                let f = self.cur_mut();
                let mut p = fs::pretty(&f.cwd);
                if !p.ends_with(std::path::MAIN_SEPARATOR) {
                    p.push(std::path::MAIN_SEPARATOR);
                }
                f.filter = p;
                f.refilter(None);
            }
            Reload if self.cur().searching() => self.restart_search(self.active, tx),
            Reload => self.reload(),
            Reveal => self.reveal_selected(),
            Palette => {
                self.close_overlays(tx);
                self.open_palette();
            }
            Jump => self.open_jump(tx),
            Search => self.open_search(tx),
            ClearJump => self.clear_jump_history(),
            SelectPattern => self.open_pick(tx),
            InvertSelection => self.invert_selection(tx),
            SelectKind => self.select_kind(tx),
            OpenSettings => self.open_settings(),
            ReloadSettings => self.reload_settings(),
            Clear => {
                if let Some(job) = &self.job {
                    job.cancel();
                    self.note = Some("Cancelling…".into());
                } else if self.cur().searching() {
                    self.search_escape(self.active, tx);
                } else {
                    self.note = None;
                    let f = self.cur_mut();
                    f.marked.clear();
                    let keep = f.cursor_name();
                    f.filter.clear();
                    f.refilter(keep.as_deref());
                    f.reveal(tx);
                }
            }
            Copy => self.copy_selected(false, tx),
            Cut => self.copy_selected(true, tx),
            // The clipboard answers a frame or more later, to this request alone: where it goes
            // and whether it moves are fixed now, by the pane it was asked in.
            Paste | PasteMove => {
                self.next_paste += 1;
                let (id, f) = (self.next_paste, self.cur());
                self.paste_reqs.push(PasteReq { id, dest: f.cwd.clone(), dest_id: f.dir_id, mv: cmd == PasteMove });
                tx.read_clipboard(move |s: &mut State, content, tx| {
                    tx.instant();
                    s.paste_reply(id, content);
                });
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
            CopyToOther => self.send_to_other_pane(false),
            MoveToOther => self.send_to_other_pane(true),
            Up => self.cur_mut().step(-1, false, tx),
            Down => self.cur_mut().step(1, false, tx),
            PageUp => self.cur_mut().step(-page, false, tx),
            PageDown => self.cur_mut().step(page, false, tx),
            First => self.cur_mut().step(isize::MIN / 2, false, tx),
            Last => self.cur_mut().step(isize::MAX / 2, false, tx),
            ExtendUp => self.cur_mut().step(-1, true, tx),
            ExtendDown => self.cur_mut().step(1, true, tx),
            ExtendPageUp => self.cur_mut().step(-page, true, tx),
            ExtendPageDown => self.cur_mut().step(page, true, tx),
            ExtendFirst => self.cur_mut().step(isize::MIN / 2, true, tx),
            ExtendLast => self.cur_mut().step(isize::MAX / 2, true, tx),
            SelectAll => self.cur_mut().select_all(),
            Hidden => {
                self.toggle_hidden();
                // Hidden folders are walked or skipped: searches start again.
                for ix in 0..self.panes.len() {
                    if self.panes[ix].f.searching() {
                        self.restart_search(ix, tx);
                    }
                }
            }
            SortName => self.sort_by(SortKey::Name, tx),
            SortKind => self.sort_by(SortKey::Kind, tx),
            SortSize => self.sort_by(SortKey::Size, tx),
            SortModified => self.sort_by(SortKey::Modified, tx),
            SortReverse => {
                let key = self.cur().sort.key;
                self.sort_by(key, tx);
            }
            NewTab => self.new_tab(tx),
            CloseTab => {
                let (ix, tab) = (self.active, self.panes[self.active].tab);
                self.close_tab(ix, tab, tx);
            }
            NextTab => self.cycle_tab(1, tx),
            PrevTab => self.cycle_tab(-1, tx),
            ReopenTab => self.reopen_tab(tx),
            Split => self.toggle_split(tx),
            OtherPane => self.activate(1 - self.active, tx),
            TogglePreview => self.toggle_preview(),
            ClosePreview => self.close_preview(),
            Place1 | Place2 | Place3 | Place4 | Place5 | Place6 | Place7 | Place8 | Place9 => {
                if let Some((_, p)) = cmd.place().and_then(|i| self.places.get(i)).cloned() {
                    self.go_to(p);
                }
            }
        }
    }

    fn sort_by(&mut self, key: SortKey, tx: &mut Tx) {
        let f = self.cur_mut();
        f.set_sort(key);
        f.reveal(tx);
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

    /// Reads the settings file again, on a worker: it's on disk, which may be slow or not a
    /// file at all, and the UI never waits on a disk. The result applies in `poll`.
    pub fn reload_settings(&mut self) {
        self.reload_settings_from(config::path());
    }

    fn reload_settings_from(&mut self, path: Option<PathBuf>) {
        self.settings_loader.request(path, |p| p.map_or_else(Config::default, |p| Config::read(&p)));
    }

    /// A reloaded settings file. Keys apply at once; a preference applies only if the file
    /// changed it, so reloading doesn't undo ⌘. or a header click made since. A new sort
    /// applies to every tab.
    fn apply_settings(&mut self, c: Config, tx: &mut Tx) {
        let (old, new) = (&self.settings, &c.settings);
        if new.sort != old.sort {
            for p in &mut self.panes {
                p.f.apply_sort(new.sort);
                for m in p.tabs.iter_mut().flatten() {
                    m.sort = new.sort;
                }
            }
        }
        if new.show_hidden != old.show_hidden {
            self.show_hidden = new.show_hidden;
            for p in &mut self.panes {
                p.f.set_hidden(new.show_hidden);
            }
        }
        if new.places != old.places {
            self.places = places_for(new);
        }
        let new = c.settings.clone();
        self.apply_history_settings(&new);
        self.reveal(tx);
        self.note = Some(c.summary().unwrap_or_else(|| "Settings reloaded".into()));
        self.keymap = c.keymap;
        self.settings = c.settings;
    }
}

impl State {
    /// `jump.enabled` and `jump.exclude`, as the settings say.
    fn apply_history_settings(&mut self, s: &Settings) {
        self.history.enabled = s.jump_enabled;
        self.history.exclude = crate::jump::exclude_globs(&s.jump_exclude);
    }

    /// Closes the palette, Jump and Select by Pattern (at most one is open).
    pub fn close_overlays(&mut self, tx: &mut Tx) {
        self.asking = None;
        self.close_palette(tx);
        self.close_jump(tx);
        self.close_pick(tx);
    }
}

/// Why a command that needs two panes can't run.
pub(crate) const NOT_SPLIT: &str = "The window isn't split";

/// Why nothing goes into a pane showing search results (which folder would it be?).
pub(crate) const IN_OTHER_SEARCH: &str = "The other pane is showing search results";

/// Re-reads `f`, keeping its selection (or selecting `names` instead). A tab still waiting for
/// the read that shows it reads again, still restoring its selection and scroll.
pub(crate) fn reload_folder(f: &mut Folder, names: Vec<OsString>, rename: bool) {
    if let Some(p) = f.pending.as_ref().filter(|p| p.nav == Nav::Show) {
        let (keep, scroll) = (if names.is_empty() { p.select.clone() } else { names }, p.scroll);
        f.request(f.cwd.clone(), Nav::Show, keep, false);
        if let Some(p) = &mut f.pending {
            p.scroll = scroll;
        }
        return;
    }
    let keep = if names.is_empty() { f.targets().iter().map(|e| e.os.clone()).collect() } else { names };
    f.request(f.cwd.clone(), Nav::Reload, keep, rename);
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
    /// The first folder, sorted by this.
    Listing(Sort, Listing),
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
            send(Boot::Listing(sort, fs::list(dir, sort)));
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
        assert!(s.is_loading() && s.cur().entries.is_empty() && s.cur().cwd == d);
        settle(&mut s);
        assert_eq!((s.cur().entries.len(), s.cur().cwd.clone()), (3, d));
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
        assert_eq!((s.cur().entries.len(), s.cur().visible.len()), (3, 3));
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
        assert!(s.show_hidden && s.cur().cwd == d);
        assert_eq!(s.cur().visible.len(), 4);
        assert!(s.cur().back.is_empty(), "the first folder isn't a navigation");
    }

    /// Polls as frames would until no pane is loading (or 10 s pass).
    pub(crate) fn settle_all(s: &mut State) {
        let until = Instant::now() + Duration::from_secs(10);
        while s.panes.iter().any(|p| p.f.is_loading()) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
            s.poll(&mut Tx::default());
        }
        assert!(!s.panes.iter().any(|p| p.f.is_loading()), "never landed");
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
        s.job = Some(ops::start(ops::Op::Copy { sources: Vec::new(), dest: d.clone() }, None, None));
        for cmd in [Cmd::Duplicate, Cmd::Trash, Cmd::Paste, Cmd::PasteMove, Cmd::Undo] {
            assert_eq!(s.unavailable(cmd), Some(BUSY), "{cmd:?}");
        }
        for cmd in [Cmd::Cancel, Cmd::Clear, Cmd::Copy, Cmd::Rename, Cmd::Down] {
            assert_eq!(s.unavailable(cmd), None, "{cmd:?}");
        }
    }

    /// Reload reads on a worker: nothing changes until the result lands in `poll`.
    #[test]
    fn reload_settings_reads_off_the_ui_thread() {
        let d = folder("reload", 1);
        std::fs::write(d.join(".h"), "x").unwrap();
        let p = d.join("config.toml");
        std::fs::write(&p, "[general]\nshow_hidden = true\n").unwrap();
        let mut s = state_in(&d);
        assert!(!s.show_hidden);
        s.reload_settings_from(Some(p));
        assert!(!s.show_hidden, "applied only when it lands");
        let until = Instant::now() + Duration::from_secs(10);
        while !s.show_hidden && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
            s.poll(&mut Tx::default());
        }
        assert!(s.show_hidden);
        assert_eq!(s.note.as_deref(), Some("Settings reloaded"));
        assert_eq!(s.cur().visible.len(), 3);
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
        assert_eq!((s.cur().entries.len(), s.cur().cwd.clone()), (2, other));
    }

    // ---- async results land only where they were asked for ---------------------------------

    /// A startup whose messages the test sends by hand.
    fn manual_boot() -> (std::sync::mpsc::Sender<Boot>, State) {
        let (send, rx) = std::sync::mpsc::channel();
        let s = State::boot(Booting { rx, waker: Default::default() }, Duration::ZERO);
        (send, s)
    }

    /// Waits for the active folder's read to land.
    fn land_now(s: &mut State) {
        settle(s);
    }

    #[test]
    fn late_settings_never_retarget_a_folder_the_user_navigated() {
        let (a, b) = (folder("retarget-a", 1), folder("retarget-b", 1));
        let (send, mut s) = manual_boot();
        s.go(b.clone(), Nav::New, None);
        land_now(&mut s);
        assert_eq!(s.target_paths(), [b.join("f0")]);
        s.reload(); // in flight when the settings land
        send.send(Boot::Start(Box::new(settings_for(a.clone(), Settings::default())))).unwrap();
        assert!(s.poll(&mut Tx::default()), "late settings redraw");
        assert_eq!(s.cur().cwd, b, "the folder on screen stays the folder files resolve against");
        assert_eq!(s.target_paths(), [b.join("f0")]);
        land_now(&mut s);
        assert_eq!(s.cur().cwd, b);
    }

    #[test]
    fn late_settings_retarget_only_the_provisional_first_folder() {
        let a = folder("provisional", 2);
        let (send, mut s) = manual_boot();
        assert!(s.is_loading() && s.cur().entries.is_empty());
        send.send(Boot::Start(Box::new(settings_for(a.clone(), Settings::default())))).unwrap();
        assert!(s.poll(&mut Tx::default()));
        assert_eq!(s.cur().cwd, a, "nothing shown yet: it waits on the start folder instead");
        assert!(s.target_paths().is_empty());
        send.send(Boot::Listing(Settings::default().sort, fs::list(a.clone(), Settings::default().sort))).unwrap();
        assert!(s.poll(&mut Tx::default()));
        assert_eq!((s.cur().entries.len(), s.is_loading()), (2, false));
    }

    #[test]
    fn late_settings_keep_a_toggle_the_user_changed() {
        let a = folder("toggle", 1);
        let (send, mut s) = manual_boot();
        s.toggle_hidden();
        s.set_sort(SortKey::Size);
        let sort = s.cur().sort;
        let file = Settings { show_hidden: false, sort: Sort { key: SortKey::Kind, desc: false }, ..Settings::default() };
        send.send(Boot::Start(Box::new(settings_for(a, file)))).unwrap();
        s.poll(&mut Tx::default());
        assert!(s.show_hidden, "the user's ⌘. stays");
        assert_eq!(s.cur().sort, sort, "and so does their sort");
    }

    /// Toggled on and off again (or a sort changed and changed back) before the settings land:
    /// still the user's choice, though it equals the default.
    #[test]
    fn late_settings_keep_a_toggle_the_user_set_back_to_the_default() {
        let a = folder("toggle-back", 1);
        let (send, mut s) = manual_boot();
        let start = s.cur().sort;
        s.toggle_hidden();
        s.toggle_hidden();
        s.set_sort(SortKey::Size);
        s.set_sort(SortKey::Name); // back to name, ascending: the default
        assert_eq!((s.show_hidden, s.cur().sort), (false, start));
        let file = Settings { show_hidden: true, sort: Sort { key: SortKey::Kind, desc: false }, ..Settings::default() };
        send.send(Boot::Start(Box::new(settings_for(a, file)))).unwrap();
        s.poll(&mut Tx::default());
        assert!(!s.show_hidden, "the user's explicit off stays");
        assert_eq!(s.cur().sort, start, "and so does their sort");
    }

    #[test]
    fn a_stale_boot_listing_never_takes_a_newer_reload() {
        let d = folder("stale-boot", 1);
        let stale = fs::list(d.clone(), Settings::default().sort);
        let (send, mut s) = manual_boot();
        send.send(Boot::Start(Box::new(settings_for(d.clone(), Settings::default())))).unwrap();
        s.poll(&mut Tx::default());
        std::fs::write(d.join("new-file"), "new").unwrap();
        s.reload();
        send.send(Boot::Listing(Settings::default().sort, stale)).unwrap();
        s.poll(&mut Tx::default());
        land_now(&mut s);
        assert_eq!(s.cur().entries.len(), 2, "the reload's fresh listing, not the startup one");
    }

    /// A listing for a request that's been superseded (the tab navigated, switched or closed)
    /// is dropped, even for the same path.
    #[test]
    fn a_late_listing_lands_only_for_its_own_request() {
        let (a, b) = (folder("late-a", 1), folder("late-b", 3));
        let mut s = state_in(&a);
        let late = |req: u64, p: &Path| Answer {
            req,
            sort: Settings::default().sort,
            listing: fs::list(p.to_path_buf(), Settings::default().sort),
        };
        // Navigated since.
        s.go(b.clone(), Nav::New, None);
        let old = s.cur().pending.as_ref().unwrap().req;
        s.go(a.clone(), Nav::New, None);
        assert!(!s.land(0, late(old, &b), &mut Tx::default()));
        land_now(&mut s);
        assert_eq!((s.cur().cwd.clone(), s.cur().entries.len()), (a.clone(), 1));
        // Switched tab since: the read for the tab that was hidden again is dropped.
        s.run(Cmd::NewTab, &mut Tx::default());
        s.go(b.clone(), Nav::New, None);
        land_now(&mut s);
        s.show_wait = Duration::ZERO;
        s.run(Cmd::PrevTab, &mut Tx::default());
        let hidden = s.cur().pending.as_ref().unwrap().req;
        s.run(Cmd::NextTab, &mut Tx::default());
        assert!(!s.land(0, late(hidden, &a), &mut Tx::default()));
        land_now(&mut s);
        assert_eq!((s.cur().cwd.clone(), s.cur().entries.len()), (b.clone(), 3));
        // Closed since: the neighbour's request is a new one.
        s.run(Cmd::PrevTab, &mut Tx::default());
        let closed = s.cur().pending.as_ref().unwrap().req;
        s.run(Cmd::CloseTab, &mut Tx::default());
        assert!(!s.land(0, late(closed, &a), &mut Tx::default()));
        land_now(&mut s);
        assert_eq!(s.cur().cwd, b);
    }

    #[test]
    fn a_listing_read_under_an_old_sort_is_sorted_again() {
        let d = folder("resort", 0);
        for (name, size) in [("a", 1), ("b", 30), ("c", 200)] {
            std::fs::write(d.join(name), "x".repeat(size)).unwrap();
        }
        let mut s = state_in(&d);
        s.reload();
        let req = s.cur().pending.as_ref().unwrap().req;
        let by_name = Settings::default().sort;
        s.set_sort(SortKey::Size); // largest first, while the read is in flight
        let a = Answer { req, sort: by_name, listing: fs::list(d.clone(), by_name) };
        assert!(s.land(0, a, &mut Tx::default()));
        let names: Vec<&str> = s.cur().entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["c", "b", "a"]);
    }

    /// Showing a tab hands its listing over within the wait, or later through `poll`; never both,
    /// never neither.
    #[test]
    fn read_waiting_hands_over_exactly_once() {
        for wait in [Duration::ZERO, Duration::from_millis(1), Duration::from_secs(5)] {
            let loader: Latest<u32> = Latest::default();
            let now = crate::folder::read_waiting(&loader, wait, || {
                std::thread::sleep(Duration::from_millis(3));
                Some(7)
            });
            let until = Instant::now() + Duration::from_secs(5);
            let mut later = None;
            while now.is_none() && later.is_none() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(1));
                later = loader.take();
            }
            std::thread::sleep(Duration::from_millis(10));
            assert_eq!(now.or(later), Some(7), "{wait:?}");
            assert_eq!(loader.take(), None, "{wait:?}: delivered once");
        }
    }
}
