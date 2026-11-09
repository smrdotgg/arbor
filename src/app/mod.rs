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

mod boot;
mod dispatch;

use boot::*;
pub use boot::{FIRST_PAINT_WAIT, SLOW_FIRST_READ_MS, read_first_folder};

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
    /// From search results: each item's identity as the walk saw it, checked when it's pasted.
    pub expect: Vec<(PathBuf, crate::fs::Ident)>,
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
    /// The note when it reports a failure (a refused undo, an operation's errors, a folder
    /// that can't be read): assistive technology announces it at once, not when idle. Only
    /// while `note` still says it.
    pub alert: Option<String>,
    pub rename: Option<Rename>,
    /// The running file operation, if any.
    pub job: Option<ops::Running>,
    pub(crate) clip: Option<Clip>,
    /// Pastes asked for whose clipboard reads haven't come back yet, oldest first.
    pub(crate) paste_reqs: Vec<PasteReq>,
    next_paste: u64,
    pub(crate) undo: Vec<ops::Undo>,
    /// Undone operations that can be redone, newest last. A new operation clears it.
    pub(crate) redo: Vec<ops::Undo>,
    /// Which keys run which command (defaults plus the settings file).
    pub keymap: Keymap,
    /// The settings as last read, so "Reload Settings" applies only what changed.
    pub(crate) settings: Settings,
    pub palette: Option<Palette>,
    /// Jump to Folder's box, while it's open (`jump.rs`).
    pub jump: Option<JumpBox>,
    /// Select by Pattern's box, while it's open (`select.rs`).
    pub pick: Option<PickBox>,
    /// Batch Rename's dialog, while it's open (`batch.rs`).
    pub batch: Option<crate::batch::BatchBox>,
    /// The rule Batch Rename last had, which it opens with (this session only).
    pub(crate) batch_rule: crate::rename::Rule,
    /// Plans Batch Rename has put on screen, for tests and timing.
    pub batch_plans: u64,
    /// The folders visited, for Jump: nothing is read until it first opens.
    pub history: History,
    /// The read a Jump asked for, so a folder that's gone is dropped from the history.
    pub(crate) jump_req: Option<u64>,
    /// The confirmation dialog, while it asks about a command that can't be undone
    /// (`confirm.rs`).
    pub asking: Option<crate::confirm::Ask>,
    /// A question a finished file operation raised (items its trash couldn't take), opened by
    /// the next `poll`.
    pub ask_next: Option<crate::confirm::Ask>,
    /// The in-window context menu, where there's no native one (Linux; `menu.rs`).
    pub menu: Option<crate::menu::Menu>,
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
    /// Where the text preview is scrolled to, and for which landed preview: its keys scroll
    /// from there (`view::preview_scroll_key`).
    pub preview_scroll: std::rc::Rc<std::cell::Cell<(u64, [f32; 2])>>,
    /// The threads searches walk on, from the first search on (kept, idle between searches).
    pub(crate) search_pool: Option<crate::search::Pool>,
    /// A go-to's look at what's at its path (on a worker), and which go-to is waited for.
    path_check: Latest<(u64, PathBuf, PathKind)>,
    pub(crate) going_to: Option<u64>,
    /// Go-to's Tab completion (`complete.rs`): the one whose card shows, the field's text a
    /// completion still on its worker was asked for, and that worker.
    pub(crate) complete: Option<crate::complete::Completion>,
    pub(crate) completing: Option<String>,
    pub(crate) completer: crate::complete::Loader,
    /// "Make Arbor the Default for Folders" or its restore, running on a worker (`handler.rs`).
    handler_job: Latest<Result<String, String>>,
    /// Requests from other apps shown so far (`incoming.rs`).
    pub(crate) external_shows: u64,
    /// The sidebar's favorites' state beyond `places` (`sidebar.rs`).
    pub side: crate::sidebar::Side,
    /// The sidebar's Locations (`volumes.rs`), read on a worker after the first frame.
    pub volumes: Vec<crate::volumes::Volume>,
    pub(crate) volume_loader: Latest<Vec<crate::volumes::Volume>>,
    /// Whether the first read of the volumes was asked for.
    volumes_asked: bool,
    pub(crate) eject_job: Latest<(String, Result<(), crate::volumes::EjectError>)>,
    /// The volume being ejected.
    pub ejecting: Option<String>,
    /// Open With's box, while it's open, and the worker reading its apps (`openwith.rs`).
    pub open_with: Option<crate::openwith::OpenWithBox>,
    pub(crate) apps_loader: crate::openwith::AppsLoader,
    pub(crate) next_open_with: u64,
    pub(crate) launch_job: Latest<Result<(), String>>,
    /// The files and apps a native context menu's Open With submenu was built with.
    pub(crate) menu_apps: Option<(Vec<PathBuf>, Vec<crate::openwith::Handler>)>,
    /// The sidebar item a menu was opened on, when it's a volume: (its index then, the volume).
    pub(crate) menu_volume: Option<(usize, crate::volumes::Volume)>,
    /// Get Info's panel, while it's open (`info.rs`).
    pub info: Option<crate::info::Info>,
    pub(crate) meta_loader: crate::info::MetaLoader,
    pub(crate) next_info: u64,
    /// The sidebar's Trash: the folder it opens (`None` on Windows). A path only.
    pub trash_place: Option<PathBuf>,
    /// What's in the Trash, counted on a thread of its own while Empty Trash's dialog is open.
    pub(crate) trash_count: Option<crate::trash::contents::Counting>,
    /// A tab being dragged: its pane, from where, and where it would land in its own strip.
    pub tab_drag: Option<(u64, usize, usize)>,
}

/// What's at a typed path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathKind {
    Folder,
    File,
    Missing,
}

/// Test only: holds a go-to's look at one path until told.
#[cfg(test)]
pub(crate) mod path_hook {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::mpsc::Receiver;

    pub(crate) static GATES: Mutex<Vec<(PathBuf, Receiver<()>)>> = Mutex::new(Vec::new());

    pub(super) fn check(p: &Path) {
        let gate = {
            let mut g = GATES.lock().unwrap();
            g.iter().position(|(q, _)| q == p).map(|i| g.remove(i))
        };
        if let Some((_, go)) = gate {
            let _ = go.recv();
        }
    }
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
        let (config, cwd, places, note, custom) = match start {
            Some(s) => (s.config, s.start, s.places, s.note, s.custom),
            None => (Config::default(), std::env::current_dir().unwrap_or_else(|_| fs::home()), Vec::new(), None, false),
        };
        let mut side = crate::sidebar::Side::new(if cfg!(test) { None } else { crate::sidebar::file_path() });
        side.custom = custom;
        side.look_due = true;
        // Headless runs have no `on_ready`: their first frame counts as shown from the start.
        side.shown = rune::tooling::headless_run().is_some();
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
            alert: None,
            rename: None,
            job: None,
            clip: None,
            paste_reqs: Vec::new(),
            next_paste: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            keymap: config.keymap,
            settings: config.settings,
            palette: None,
            jump: None,
            pick: None,
            batch: None,
            batch_rule: crate::rename::Rule::default(),
            batch_plans: 0,
            history,
            jump_req: None,
            asking: None,
            ask_next: None,
            menu: None,
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
            preview_scroll: Default::default(),
            search_pool: None,
            path_check: Latest::default(),
            going_to: None,
            complete: None,
            completing: None,
            completer: Latest::default(),
            handler_job: Latest::default(),
            external_shows: 0,
            side,
            volumes: Vec::new(),
            volume_loader: Latest::default(),
            // Tests read the volumes when they ask (the first poll's read would make polls
            // report changes at random).
            volumes_asked: cfg!(test),
            eject_job: Latest::default(),
            ejecting: None,
            open_with: None,
            apps_loader: Latest::default(),
            next_open_with: 0,
            launch_job: Latest::default(),
            menu_apps: None,
            menu_volume: None,
            info: None,
            meta_loader: Latest::default(),
            next_info: 0,
            trash_place: crate::trash::contents::place(),
            trash_count: None,
            tab_drag: None,
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
                    picks: 0,
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
        self.history.waker = Some(waker.clone());
        self.path_check.set_waker(waker.clone());
        self.completer.set_waker(waker.clone());
        self.handler_job.set_waker(waker.clone());
        self.volume_loader.set_waker(waker.clone());
        self.eject_job.set_waker(waker.clone());
        self.apps_loader.set_waker(waker.clone());
        self.launch_job.set_waker(waker.clone());
        self.meta_loader.set_waker(waker.clone());
        self.side.dropped.set_waker(waker.clone());
        if let Some(b) = &self.boot
            && let Ok(mut w) = b.waker.lock()
        {
            *w = Some(waker.clone());
        }
        self.waker = Some(waker);
    }

    /// The active pane's folder.
    /// The startup read hasn't landed yet.
    pub(crate) fn booting(&self) -> bool {
        self.boot.is_some()
    }

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
        if let Some(a) = self.ask_next.take() {
            self.ask_with(a, tx);
            changed = true;
        }
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
        if let Some(a) = self.path_check.take() {
            self.went_path(a);
            changed = true;
        }
        if let Some(r) = self.handler_job.take() {
            self.note = Some(r.unwrap_or_else(|e| e));
            // Linux: the running Arbor serves "Show in folder" requests from now on, or stops.
            #[cfg(all(unix, not(target_os = "macos")))]
            crate::incoming::linux::recheck();
            changed = true;
        }
        // The sidebar's volumes, and one look at each favorite, from the first frame on: never
        // before it, and never on this thread.
        if !self.volumes_asked {
            self.volumes_asked = true;
            self.refresh_volumes();
        }
        if self.side.look_due && self.side.shown && self.boot.is_none() {
            self.side.look_due = false;
            self.look_at_favorites();
        }
        changed |= self.poll_trash_count();
        changed |= self.poll_sidebar();
        changed |= self.poll_volumes();
        changed |= self.poll_open_with();
        changed |= self.poll_info();
        changed |= self.poll_completion();
        changed |= self.poll_history();
        changed |= self.poll_search();
        changed |= self.poll_batch();
        // Select by Pattern's count follows the rows it counts (a listing that landed).
        changed |= self.pick_refresh();
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
        let Start { config, start, places, note, custom } = start;
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
        self.side.custom = custom;
        self.side.look_due = true;
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
        let Some(mut p) = f.pending.take_if(|p| p.req == a.req && p.path == a.listing.path) else { return false };
        // A reload (after a rename, a copy, a new folder) asked to select what the operation
        // made; if the user picked something else meanwhile, their choice stands, so a ⌘C right
        // after a click copies what was clicked.
        if p.nav == Nav::Reload && f.picks != p.picks {
            p.select = f.picked();
            p.rename = false;
        }
        let jumped = self.jump_req.take_if(|&mut r| r == p.req).is_some();
        let placed = self.side.going.take_if(|(r, _)| *r == p.req).is_some();
        match a.listing.result {
            Err(_) if jumped && a.listing.missing => self.jump_missed(&p.path),
            Err(_) if placed && a.listing.missing => self.place_missed(&p.path),
            // The sidebar's Trash: Linux makes its folder the first time something goes in it,
            // and macOS needs Full Disk Access to read it.
            Err(e) if self.trash_place.as_ref() == Some(&p.path) => {
                self.note = Some(if a.listing.missing {
                    format!("The {} is empty", crate::trash::BIN)
                } else if cfg!(target_os = "macos") && e.contains("not permitted") {
                    crate::trash::contents::NEEDS_ACCESS.to_string()
                } else {
                    format!("{}: {e}", fs::pretty(&p.path))
                });
                self.alert = self.note.clone();
            }
            Err(e) => {
                self.note = Some(format!("{}: {e}", fs::pretty(&p.path)));
                self.alert = self.note.clone();
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
                    Nav::Fresh => {
                        f.back.clear();
                        f.fwd.clear();
                    }
                    Nav::Reload | Nav::Show => {}
                }
                f.entries = entries;
                f.now = crate::folder::now();
                let navigated = matches!(p.nav, Nav::New | Nav::Back | Nav::Forward | Nav::Fresh);
                if navigated {
                    f.filter.clear();
                    // For Jump: one message to the history's writer, no disk I/O here.
                    let here = f.cwd.clone();
                    self.history.record(&here);
                }
                let f = &mut self.panes[ix].f;
                f.marked.clear();
                if p.select.len() > 1 {
                    // Only names still here: what was trashed or deleted isn't selected.
                    let here: std::collections::HashSet<&OsString> = f.entries.iter().map(|e| &e.os).collect();
                    f.marked.extend(p.select.iter().filter(|n| here.contains(n)).cloned());
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
        let targets = typed_targets(&self.cur().filter);
        self.go_targets(targets);
    }

    /// Go-to: a folder navigates, a file opens, anything else says it isn't there. What's at
    /// the path is looked up on a worker (a typed path can be on a slow or gone disk), waiting
    /// here at most a frame as showing a tab does; a slower answer acts when it lands, unless
    /// another go-to was asked for since.
    pub(crate) fn go_path(&mut self, path: &Path) {
        self.go_targets(vec![path.to_path_buf()]);
    }

    /// [`go_path`](Self::go_path) to the first of `targets` that's there (each a reading of
    /// what was typed: [`typed_targets`]); "isn't there" names the first.
    fn go_targets(&mut self, targets: Vec<PathBuf>) {
        let id = crate::folder::next_request();
        self.going_to = Some(id);
        let wait = self.show_wait;
        let answer = crate::bounded::read_waiting(&self.path_check, wait, move || {
            let kind = |path: &Path| {
                #[cfg(test)]
                path_hook::check(path);
                if path.is_dir() {
                    PathKind::Folder
                } else if path.exists() {
                    PathKind::File
                } else {
                    PathKind::Missing
                }
            };
            let found = targets.iter().map(|p| (p, kind(p))).find(|(_, k)| *k != PathKind::Missing);
            let (path, kind) = found.map_or_else(|| (targets[0].clone(), PathKind::Missing), |(p, k)| (p.clone(), k));
            Some((id, path, kind))
        });
        if let Some(a) = answer {
            self.went_path(a);
        }
    }

    /// What a go-to found, if it's still the one asked for.
    fn went_path(&mut self, (id, path, kind): (u64, PathBuf, PathKind)) {
        if self.going_to != Some(id) {
            return;
        }
        self.going_to = None;
        match kind {
            PathKind::Folder => self.go(path, Nav::New, None),
            PathKind::File => {
                if let Err(e) = fs::open(&path) {
                    self.note = Some(format!("Couldn't open: {e}"));
                }
            }
            PathKind::Missing => self.note = Some(format!("No such folder: {}", path.display())),
        }
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

    /// Makes Arbor the default for folders, or puts the old default back, on a worker: it
    /// writes files, the registry or LaunchServices. Confirmed in the dialog first.
    pub fn change_default(&mut self, change: crate::handler::Change) {
        self.handler_job.request(change, crate::handler::run);
        self.note = Some(match change {
            crate::handler::Change::Make => "Making Arbor the default for folders…".into(),
            crate::handler::Change::Restore => "Restoring the system default for folders…".into(),
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
        // The settings' places are the default list: the user's own (sidebar.toml) stays.
        if new.places != old.places && !self.side.custom {
            self.places = places_for(new);
            self.side.look_due = true;
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

    /// The window is closing: every search walk and recheck stops where it is, and Batch
    /// Rename's folder read, before the last moment given to the Jump history's writer.
    pub fn stop_background(&mut self) {
        for p in &mut self.panes {
            if let Some(s) = p.f.search.as_mut() {
                s.stop_all();
            }
        }
        self.close_batch(&mut Tx::default());
        self.info = None;
    }

    /// Closes the palette, Jump, Select by Pattern and Batch Rename (at most one is open).
    pub fn close_overlays(&mut self, tx: &mut Tx) {
        self.asking = None;
        self.menu = None;
        self.close_palette(tx);
        self.close_jump(tx);
        self.close_pick(tx);
        self.close_batch(tx);
        self.close_open_with();
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

pub fn is_path_like(s: &str) -> bool {
    is_path_like_on(s, cfg!(windows))
}

/// [`is_path_like`] as on Windows (`windows`: a drive, `C:`, starts one too) or elsewhere.
pub fn is_path_like_on(s: &str, windows: bool) -> bool {
    let b = s.as_bytes();
    s.starts_with('/')
        || s.starts_with('~')
        || s.starts_with('\\')
        || (windows && b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

/// What a typed path (go-to) can mean, in order: exactly as typed (leading spaces aside), then,
/// if it ends in spaces, without them. A name can end in a space (Tab completion writes it
/// whole), so the exact path comes first; one typed with a stray space still goes where meant.
pub fn typed_targets(text: &str) -> Vec<PathBuf> {
    let exact = text.trim_start();
    let mut v = vec![fs::expand(exact)];
    let trimmed = exact.trim_end();
    if trimmed.len() < exact.len() && !trimmed.is_empty() {
        v.push(fs::expand(trimmed));
    }
    v
}

pub fn title(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
pub(crate) mod tests;
