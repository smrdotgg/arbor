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
use crate::palette::Palette;
use crate::preview::{self, Preview};
use crate::select::PickBox;

mod boot;
mod dispatch;
mod shared;

use boot::*;
pub use boot::{FIRST_PAINT_WAIT, SLOW_FIRST_READ_MS, read_first_folder};
pub(crate) use shared::Job;
pub use shared::Shared;

/// Rows a Page Up / Page Down moves.
const PAGE: usize = 20;

/// Why a command that starts a file operation can't run while another one does.
pub(crate) const BUSY: &str = "Another file operation is running";

/// [`BUSY`], when the operation is another window's (only that window shows and cancels it).
pub(crate) const BUSY_ELSEWHERE: &str = "Another file operation is running in another window";

/// An inline rename in progress (always in the active pane).
pub struct Rename {
    pub os: OsString,
    pub edit: TextEdit,
}

/// What ⌘C / ⌘X put aside, so a paste knows a cut means move.
pub(crate) struct Clip {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
    /// Each item's identity as its listing or the search walk saw it, checked when it's pasted.
    pub expect: crate::ops::Checks,
}

/// ⌘V or ⌥⌘V, waiting for the clipboard: the folder that was on screen in the active pane
/// when it was asked for (its path and its identity, from its listing), and whether it moves.
/// Each request has its own, answered by its own clipboard read (`id`), so two pastes asked
/// before either answer each go where, and as, they were asked.
pub(crate) struct PasteReq {
    pub id: u64,
    pub dest: PathBuf,
    pub dest_id: Option<crate::noclobber::DirId>,
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
    /// Which window this is (`window::FIRST` for the one Arbor opens with).
    pub win: u64,
    /// What every window shares: the file-operation slot, undo and redo, the clipboard's
    /// files, the Jump history (`app/shared.rs`).
    pub shared: std::rc::Rc<Shared>,
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
    /// Folders above files in every tab's sort ("Folders First", `folders_first`), and whether
    /// the user toggled it since the window opened (late settings then leave it alone).
    pub folders_first: bool,
    folders_set: bool,
    /// A column header's edge being dragged: which column, its width when the drag began, and
    /// how much the name could give then.
    pub(crate) head_drag: Option<(crate::listcols::Field, f32, f32)>,
    pub places: Vec<(String, PathBuf)>,
    /// The last error or note, shown in the status bar until the next action.
    pub note: Option<String>,
    /// The note when it reports a failure (a refused undo, an operation's errors, a folder
    /// that can't be read): assistive technology announces it at once, not when idle. Only
    /// while `note` still says it.
    pub alert: Option<String>,
    /// The file operation's outcome `note` says, by number, while it may still say it: an undo
    /// of that outcome (from any window) takes the note away (`State::outcome_undone`).
    pub(crate) note_of: Option<(u64, String)>,
    /// What the last quit left partly done (`leftovers.rs`), while the note saying so shows:
    /// its Show button reveals it.
    pub leftover: Option<(PathBuf, String)>,
    /// The leftovers record as the startup read found it, until the note saying it has been
    /// shown: then what was read is removed from it (`State::forget_leftovers`).
    pub(crate) leftovers_seen: Option<crate::leftovers::Seen>,
    /// Where closing the last window records what a cut-short operation left, and how long its
    /// cleanup gets (`leftovers.rs`).
    pub(crate) quit_record: Option<PathBuf>,
    pub(crate) quit_wait: std::time::Duration,
    pub rename: Option<Rename>,
    /// The item New File or New Folder just made, and its identity when it could be read: its
    /// rename starts once a reload lists it selected, and only on it (`State::rename_made`).
    pub(crate) made: Option<(PathBuf, Option<fs::Ident>)>,
    /// Pastes asked for whose clipboard reads haven't come back yet, oldest first.
    pub(crate) paste_reqs: Vec<PasteReq>,
    next_paste: u64,
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
    /// Where this frame's Shift + wheel deltas have taken the text preview sideways so far (for
    /// which landed preview): `preview_scroll` only catches up after the frame, so several
    /// notches in one frame add up from here (`view::preview_shift_wheel`). The view clears it.
    pub preview_turn: std::cell::Cell<Option<(u64, f32)>>,
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
    /// A drag-select rectangle being dragged out (`band.rs`).
    pub band: Option<crate::band::Band>,
    /// The last rectangle moved: the press its row gets on release isn't a click.
    pub(crate) band_click: bool,
    /// How big the icon view's icons are, for every pane (⌘+ and ⌘-; `view.icon_size`).
    pub icon_size: crate::grid::IconSize,
    /// The user chose a view or icon size since the window opened, so settings that land late
    /// at startup leave them alone.
    pub(crate) view_chosen: bool,
    /// The icon view's thumbnail workers, from the first grid with something to thumbnail.
    pub(crate) tile_pool: Option<crate::tiles::Pool>,
    /// Device pixels per point, written by the view: thumbnails decode to the icon's side in
    /// them ([`State::tile_fit`]).
    pub(crate) ui_scale: std::cell::Cell<f32>,
    /// The column view's column width, for every pane (its dividers drag it; this session).
    pub col_w: f32,
    /// The width when a divider drag started, while one is dragged.
    pub(crate) col_resize: Option<f32>,
    /// How long a change waits for the columns it needs to be read (`columns::WAIT`).
    pub(crate) column_wait: std::time::Duration,
    /// A column made the active folder, its listing in hand, waiting for a `Tx` to land
    /// ([`State::flush_columns`]): the pane, and the answer.
    pub(crate) col_ready: Option<(u64, Answer)>,
    /// Columns to scroll to their highlighted item (rows key, row key), with the next `Tx`.
    pub(crate) col_reveals: Vec<(String, String)>,
    /// The open preview is the column view's own (drawn as its last column, opened and closed
    /// with the selection), not the docked panel Space opens.
    pub col_preview: bool,
    /// Space (or Close Preview) turned the column view's preview off, this session.
    pub col_preview_off: bool,
    /// Folder reads the column view started (its columns', and a column made active before its
    /// listing was in), for timing: a key that started one is a move that reads.
    pub col_reads: u64,
    /// Column reads that landed through `poll` (timing: their frame is no key's).
    pub col_lands: u64,
}

/// What's at a typed path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathKind {
    Folder,
    File,
    Missing,
    /// Show Original: the item a symbolic link points at, there to select.
    Original,
    /// Show Original: the same, a hidden item (a dot name, or Windows' hidden attribute).
    HiddenOriginal,
    /// Show Original: the link points at nothing (or couldn't be read).
    Broken,
}

/// Where the symbolic link at `link` points: the original's path, and whether something is
/// there. Spelled as written (Arbor's own links are absolute) unless it has `.` or `..` in it:
/// then those are resolved, on Unix through the folders' real paths, as the system follows a
/// link (kept in the link's own spelling when that lands in the link's folder), and on Windows
/// by their spelling, as Windows resolves them. The original itself is kept as it is, a link
/// too perhaps (Show Original on it goes on).
pub(crate) fn original(link: &Path) -> (PathBuf, PathKind) {
    use std::path::Component;
    let Ok(target) = std::fs::read_link(link) else { return (link.to_path_buf(), PathKind::Broken) };
    let here = link.parent().unwrap_or(Path::new(""));
    let abs = if target.is_absolute() { target } else { here.join(target) };
    let resolved = if !abs.components().any(|c| matches!(c, Component::CurDir | Component::ParentDir)) {
        Some(abs.clone())
    } else if cfg!(unix) {
        match (abs.parent(), abs.file_name()) {
            (Some(dir), Some(name)) => std::fs::canonicalize(dir)
                .ok()
                .map(|d| if std::fs::canonicalize(here).is_ok_and(|h| h == d) { here.join(name) } else { d.join(name) }),
            _ => std::fs::canonicalize(&abs).ok(),
        }
    } else {
        Some(lexical(&abs))
    };
    let Some((p, m)) = resolved.and_then(|p| p.symlink_metadata().ok().map(|m| (p, m))) else { return (abs, PathKind::Broken) };
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let kind = if fs::is_hidden(&name, Some(&m)) { PathKind::HiddenOriginal } else { PathKind::Original };
    (p, kind)
}

/// `p` with `.` and `..` taken out by their spelling.
fn lexical(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(c);
                }
            }
            c => out.push(c),
        }
    }
    out
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
        let (config, cwd, places, note, custom, leftover, cols) = match start {
            Some(mut s) => {
                let seen = s.leftovers.take();
                (s.config, s.start, s.places, s.note, s.custom, seen, Some(s.cols))
            }
            None => (Config::default(), std::env::current_dir().unwrap_or_else(|_| fs::home()), Vec::new(), None, false, None, None),
        };
        let mut side = crate::sidebar::Side::new(if cfg!(test) { None } else { crate::sidebar::file_path() });
        side.custom = custom;
        side.look_due = true;
        // Headless runs have no `on_ready`: their first frame counts as shown from the start.
        side.shown = rune::tooling::headless_run().is_some();
        // Where the history lives (environment only, no disk): tests keep theirs in memory.
        let mut history = History::new(if cfg!(test) { None } else { crate::jump::log_path() });
        history.enabled = config.settings.jump_enabled;
        history.exclude = crate::jump::exclude_globs(&config.settings.jump_exclude);
        let shared = std::rc::Rc::new(Shared::new(history));
        if let Some(c) = cols {
            shared.cols.set(c);
        }
        *shared.cols_file.borrow_mut() = if cfg!(test) { None } else { crate::listcols::file_path() };
        let note = config.summary().or(note);
        let mut s = State::assemble(crate::window::FIRST, shared, config.settings, config.keymap, cwd, places, side);
        s.note = note;
        s.take_leftovers(leftover);
        let f = &mut s.panes[0].f;
        match listing {
            Some(first) => match first.result {
                Ok(v) => {
                    f.entries = std::sync::Arc::new(v);
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
                    cursor: None,
                    none: false,
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

    /// A window's state around its first folder (not read yet), with the settings, keys and
    /// favorites it starts with, sharing `shared` with the other windows.
    fn assemble(
        win: u64,
        shared: std::rc::Rc<Shared>,
        settings: Settings,
        keymap: Keymap,
        cwd: PathBuf,
        places: Vec<(String, PathBuf)>,
        side: crate::sidebar::Side,
    ) -> State {
        let mut first = Folder::new(0, cwd, settings.sort, settings.show_hidden, None);
        first.view = settings.view_default;
        let icon_size = settings.icon_size;
        shared.join(win);
        State {
            win,
            shared,
            panes: vec![Pane { f: first, tabs: vec![None], tab: 0 }],
            active: 0,
            closed: Vec::new(),
            next_pane: 1,
            show_wait: FIRST_PAINT_WAIT,
            lands: 0,
            pane_drop: None,
            show_hidden: settings.show_hidden,
            hidden_set: false,
            folders_first: settings.sort.dirs_first,
            folders_set: false,
            head_drag: None,
            places,
            note: None,
            alert: None,
            note_of: None,
            leftover: None,
            leftovers_seen: None,
            quit_record: if cfg!(test) { None } else { crate::leftovers::file() },
            quit_wait: crate::leftovers::QUIT_WAIT,
            rename: None,
            made: None,
            paste_reqs: Vec::new(),
            next_paste: 0,
            keymap,
            settings,
            palette: None,
            jump: None,
            pick: None,
            batch: None,
            batch_rule: crate::rename::Rule::default(),
            batch_plans: 0,
            jump_req: None,
            asking: None,
            ask_next: None,
            menu: None,
            palette_sink: TextEdit::new(""),
            rename_sink: TextEdit::new(""),
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
            preview_turn: Default::default(),
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
            band: None,
            band_click: false,
            icon_size,
            view_chosen: false,
            tile_pool: None,
            ui_scale: std::cell::Cell::new(1.0),
            col_w: crate::columns::COL_W,
            col_resize: None,
            column_wait: crate::columns::WAIT,
            col_ready: None,
            col_reveals: Vec::new(),
            col_preview: false,
            col_preview_off: false,
            col_reads: 0,
            col_lands: 0,
        }
    }

    /// Another window's state, sharing this one's (`app/shared.rs`), on `path` with one tab:
    /// this window's settings, keys, favorites and volumes, and nothing read from the disk but
    /// the folder. Its read starts here and isn't waited for: creating the native window takes
    /// longer than a frame anyway (50–80 ms under Xvfb), so the listing lands in the new window's
    /// `setup` when it's in by then, else in its `poll`, usually before its first frame. Hidden
    /// files and the icon size follow this window; the view starts as the settings say, as a
    /// new tab's does.
    pub fn new_window(&self, path: PathBuf) -> State {
        let side = self.side.for_new_window();
        let mut s = State::assemble(
            crate::window::next_id(),
            self.shared.clone(),
            self.settings.clone(),
            self.keymap.clone(),
            path.clone(),
            self.places.clone(),
            side,
        );
        s.show_hidden = self.show_hidden;
        s.hidden_set = self.hidden_set;
        s.set_folders_first(self.folders_first);
        s.folders_set = self.folders_set;
        s.icon_size = self.icon_size;
        s.view_chosen = self.view_chosen;
        s.volumes = self.volumes.clone();
        s.volumes_asked = true;
        s.show_wait = self.show_wait;
        s.quit_record.clone_from(&self.quit_record);
        s.quit_wait = self.quit_wait;
        s.panes[0].f.set_hidden(self.show_hidden);
        let f = &mut s.panes[0].f;
        f.pending = Some(crate::folder::Pending {
            req: crate::folder::next_request(),
            path,
            nav: Nav::Fresh,
            select: Vec::new(),
            cursor: None,
            none: false,
            rename: false,
            scroll: 0.0,
            picks: 0,
        });
        // Tests and headless runs wait, so what they check is there.
        let wait = if cfg!(test) || rune::tooling::headless_run().is_some() { s.show_wait } else { std::time::Duration::ZERO };
        if let Some(a) = s.panes[0].f.request_now(wait) {
            s.land(0, a, &mut Tx::default());
        }
        // Its title, from its first `poll` (the read that landed here had no window to name).
        s.shared.send(s.win, Box::new(|s, tx| tx.set_title(title(&s.cur().cwd))));
        s
    }

    /// What the startup read found in the leftovers record: what's still there is said first
    /// (over the settings' own note), and the record is kept until that's been shown.
    fn take_leftovers(&mut self, seen: Option<crate::leftovers::Seen>) {
        let Some(seen) = seen else { return };
        if let Some(said) = crate::leftovers::note(&seen.offered) {
            self.leftover = seen.offered.first().map(|l| (l.path.clone(), said.clone()));
            self.note = Some(said);
        }
        self.leftovers_seen = Some(seen);
    }

    /// The leftover note has been shown (its frame presented, or the user typed or started an
    /// operation since): what the startup read found leaves the record, off the UI thread.
    pub(crate) fn forget_leftovers(&mut self) {
        if let Some(seen) = self.leftovers_seen.take() {
            drop(crate::leftovers::forget(seen));
        }
    }

    /// The leftover note's Show button: the item, selected in its folder.
    pub fn show_leftover(&mut self) {
        self.forget_leftovers();
        let Some((p, _)) = self.leftover.take() else { return };
        self.note = None;
        if let (Some(dir), Some(name)) = (p.parent(), p.file_name()) {
            self.go_full(dir.to_path_buf(), Nav::New, vec![name.to_os_string()], false);
        }
    }

    /// ⌘N and "Open in New Window": another window on `path` (`window::open`).
    pub fn open_window(&mut self, path: PathBuf) {
        let s = self.new_window(path);
        crate::window::open(s, self.win);
    }

    /// The folder "Open in New Window" opens: the one item selected, when it's a folder.
    pub(crate) fn folder_to_open(&self) -> Option<PathBuf> {
        let f = self.cur();
        match f.targets().as_slice() {
            [e] if e.is_dir && e.pending.is_none() => Some(f.cwd.join(&e.os)),
            _ => None,
        }
    }

    pub fn setup(&mut self, waker: rune::Waker) {
        crate::draglog::install();
        // A new window's folder, read while the native window was being made: shown in its first
        // frame when it's in (`new_window`).
        if self.win != crate::window::FIRST
            && let Some(a) = self.panes[0].f.loader.take()
        {
            self.land(0, a, &mut Tx::default());
        }
        for p in &self.panes {
            p.f.loader.set_waker(waker.clone());
            p.f.free_inbox.set_waker(waker.clone());
        }
        self.settings_loader.set_waker(waker.clone());
        self.shared.set_waker(self.win, waker.clone());
        // Made the operation's owner before it was set up (its opener closed first): the
        // worker wakes it from now on.
        if self.owns_job()
            && let Ok(mut w) = self.shared.op_waker.lock()
        {
            *w = Some(waker.clone());
        }
        {
            // The Jump history's writer is every window's: it wakes the open window focused
            // last when a write failed or a clear finished, never one that has closed.
            let mut h = self.shared.history.borrow_mut();
            if h.waker.is_none() {
                h.waker = Some(self.shared.live_waker());
            }
        }
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
        // The first folder's volume's free space, when its listing was in before the window
        // (the startup read): one call on a worker, now that its answer can wake the window.
        for p in &mut self.panes {
            if p.f.pending.is_none() && p.f.free.is_none() {
                p.f.ask_free();
            }
        }
        // A window opening as columns (`view.default`) gets them from its first frame.
        self.sync_columns();
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
        // In the column view, a folder on screen as a column becomes the active one from its
        // listing (no read).
        if !rename && matches!(nav, Nav::New | Nav::Back | Nav::Forward) && self.adopt_column(&path, nav, &select) {
            return;
        }
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
        crate::draglog::poll();
        // What other windows sent (a refresh after their operation, new settings or favorites)
        // first: a file operation handed over by a window that closed lands just below.
        let mut changed = self.poll_inbox(tx);
        changed |= self.poll_job();
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
        // Free space, asked when a folder's listing landed: only an answer for the folder the
        // active pane shows changes the status bar.
        for ix in 0..self.panes.len() {
            let f = &mut self.panes[ix].f;
            if let Some((path, free)) = f.free_inbox.take() {
                let shown = ix == self.active && path == f.cwd && !f.is_loading();
                changed |= shown && f.free.as_ref() != Some(&(path.clone(), free));
                f.free = Some((path, free));
            }
        }
        if let Some(c) = self.settings_loader.take() {
            // Every window takes them; this one says so.
            let shared = std::rc::Rc::new(c.clone());
            self.shared.tell_others(self.win, || {
                let c = shared.clone();
                Box::new(move |s, tx| s.apply_settings((*c).clone(), false, tx))
            });
            self.apply_settings(c, true, tx);
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
        changed |= self.poll_cols_writer();
        changed |= self.poll_volumes();
        changed |= self.poll_open_with();
        changed |= self.poll_info();
        changed |= self.poll_completion();
        changed |= self.poll_history();
        changed |= self.poll_search();
        changed |= self.poll_batch();
        // Select by Pattern's count follows the rows it counts (a listing that landed).
        changed |= self.pick_refresh();
        changed |= self.poll_columns();
        changed |= self.sync_tiles();
        changed |= self.poll_tiles();
        // After everything that can move the selection this frame.
        changed |= self.sync_preview();
        changed |= self.land_preview();
        if self.col_ready.is_some() || !self.col_reveals.is_empty() {
            self.flush_columns(tx);
            changed = true;
        }
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
                        changed |= self.land(ix, Answer::new(req, sort, listing), tx);
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
        let Start { config, start, places, note, custom, leftovers, cols } = start;
        if leftovers.is_some() {
            self.land_start(Start { config, start, places, note, custom, leftovers: None, cols });
            self.take_leftovers(leftovers);
            return;
        }
        // The columns, unless the user changed them already.
        if !self.shared.cols_set.get() {
            self.shared.cols.set(cols);
        }
        if let Some(req) = self.boot_req
            && let Some(f) = self.panes.iter_mut().map(|p| &mut p.f).find(|f| f.pending.as_ref().is_some_and(|p| p.req == req))
            && f.entries.is_empty()
            && let Some(p) = f.pending.as_mut()
        {
            p.path = start.clone();
            f.cwd = start;
        }
        let (old, new) = (self.settings.clone(), config.settings.clone());
        if !self.hidden_set {
            self.show_hidden = new.show_hidden;
            for p in &mut self.panes {
                p.f.set_hidden(new.show_hidden);
            }
        }
        if !self.folders_set && new.sort.dirs_first != self.folders_first {
            self.set_folders_first(new.sort.dirs_first);
        }
        // Folders First is the window's (above); the key and direction each tab's.
        let sort = fs::Sort { dirs_first: self.folders_first, ..new.sort };
        let changed = (new.sort.key, new.sort.desc) != (old.sort.key, old.sort.desc);
        for p in &mut self.panes {
            if !p.f.sort_set && changed {
                p.f.apply_sort(sort);
            }
            for m in p.tabs.iter_mut().flatten().filter(|m| !m.sort_set) {
                m.sort = sort;
            }
        }
        // The view settings, where the user hasn't chosen since the window opened.
        if !self.view_chosen {
            for p in &mut self.panes {
                p.f.view = new.view_default;
            }
            self.icon_size = new.icon_size;
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
    pub(crate) fn land(&mut self, ix: usize, mut a: Answer, tx: &mut Tx) -> bool {
        let active = ix == self.active;
        let f = &mut self.panes[ix].f;
        let Some(mut p) = f.pending.take_if(|p| p.req == a.req && p.path == a.listing.path) else { return false };
        // A column made active brings its element and rows, for its own request only.
        let adopted = f.adopted.take().filter(|d| d.req == p.req);
        let (adopt_uid, adopt_rows) = adopted.map_or((None, None), |d| (Some(d.uid), d.rows));
        // A reload (after a rename, a copy, a new folder) asked to select what the operation
        // made; if the user picked something else meanwhile, their choice stands, so a ⌘C right
        // after a click copies what was clicked.
        if p.nav == Nav::Reload && f.picks != p.picks {
            p.rename = false;
            p.picks = f.picks;
            (p.select, p.cursor, p.none) = (Vec::new(), None, f.selected().is_none());
            // The selection on screen now, found in the new listing on a worker (handed over as
            // it is: looking through a big folder, or even copying its names, here would hold
            // up the window). Then it lands, unless the user picks again meanwhile.
            if a.listing.result.is_ok() {
                let kept = f.kept();
                f.pending = Some(p);
                f.loader.request_opt((), move |()| {
                    (a.marks, a.chosen) = (None, None);
                    a.pick(crate::folder::Pick::Kept(kept));
                    Some(a)
                });
                return false;
            }
        }
        // What a worker worked out from the selection kept on screen: the names, the cursor's.
        if let Some((select, cursor)) = a.chosen.take() {
            (p.select, p.cursor) = (select, cursor);
        }
        let mut marks = a.marks.take();
        // The folder on screen couldn't be read again (a reload, a tab shown): what it listed
        // before isn't shown or acted on any more, only why it can't be read. (A navigation
        // elsewhere that fails leaves the folder it was in, as it was.)
        if a.listing.result.is_err() && p.path == f.cwd {
            f.entries = std::sync::Arc::default();
            f.visible = std::sync::Arc::default();
            f.marked.clear();
            f.sel = 0;
            f.anchor = 0;
            f.dir_id = None;
            f.bump();
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
                    match marks.as_mut() {
                        // What's to be selected moves with its entries.
                        Some(m) => crate::folder::sort_carrying(&mut entries, m, f.sort),
                        None => fs::sort_entries(&mut entries, f.sort),
                    }
                }
                let old = std::mem::replace(&mut f.cwd, p.path.clone());
                let old_id = std::mem::replace(&mut f.dir_id, a.listing.dir_id);
                let moved = old != f.cwd;
                let left_from = (moved && f.cols.is_some()).then(|| old.clone());
                if moved {
                    // Another folder: its visible rows are Rune's to say, and no row of it is
                    // under a drag yet (a name is no row of another folder).
                    f.grid_rows = None;
                    f.drop_target = None;
                }
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
                // The rows the folder showed, for the column it becomes (when they're its whole
                // listing, no filter): kept with their version rather than worked out again.
                let own = |r: std::sync::Arc<crate::marked::Rows>| std::sync::Arc::try_unwrap(r).unwrap_or_else(|r| (*r).clone());
                let old_rows = (left_from.is_some() && f.filter.is_empty()).then(|| (own(std::mem::take(&mut f.visible)), f.version));
                let old_entries = std::mem::replace(&mut f.entries, std::sync::Arc::new(entries));
                // Its own unless a reload's worker still holds it (then a copy).
                let old_entries = std::sync::Arc::try_unwrap(old_entries).unwrap_or_else(|a| (*a).clone());
                // The column view: the folder just left can be a column in the same frame (on
                // the right after going up, on the left after going in), from this listing.
                if let Some(old) = left_from
                    && let Some(cols) = f.cols.as_mut()
                    && !old_entries.is_empty()
                {
                    // The column left takes the element the folder was drawn as; the folder, the
                    // one of the column it came from (`adopt_column`), or a new one.
                    let uid = f.col_uid.replace(adopt_uid.unwrap_or_else(crate::columns::next_uid));
                    let col = crate::columns::Column::ready(
                        old,
                        old_entries,
                        old_id,
                        (f.sort, f.show_hidden),
                        uid,
                        old_rows,
                        self.waker.as_ref(),
                    );
                    // Its element keeps its scroll; so does the folder, drawn as the column's.
                    col.scroll.set(f.scroll.get());
                    cols.spare.push(col);
                }
                f.now = crate::folder::now();
                let navigated = matches!(p.nav, Nav::New | Nav::Back | Nav::Forward | Nav::Fresh);
                if navigated {
                    f.filter.clear();
                    f.goto = false;
                    // For Jump: one message to the history's writer, no disk I/O here.
                    let here = f.cwd.clone();
                    self.shared.history.borrow_mut().record(&here);
                }
                let f = &mut self.panes[ix].f;
                // The rows before were the old listing's.
                f.marked.clear();
                // Only names still here are selected: what was trashed or deleted isn't. The
                // listing's worker marked them on their entries when this is what it was asked
                // to select (a column made active, the only other way with two or more, is
                // marked here).
                let several = p.marks();
                if several && marks.is_none() {
                    marks = Some(crate::folder::mark_named(&f.entries, &p.select));
                }
                f.carry = if several { marks.unwrap_or_default() } else { Default::default() };
                let mut survived = false;
                match adopt_rows {
                    // A column's rows, as it showed them (same sort, no filter, the hidden-file
                    // rule it was shown with): kept, under their version.
                    Some((visible, version)) if a.sort == f.sort && f.filter.is_empty() && !several => {
                        f.visible = std::sync::Arc::new(visible);
                        f.version = version;
                        let pick = p.cursor_name();
                        f.sel = pick.and_then(|n| f.visible.iter().position(|&e| f.entries[e as usize].os == *n)).unwrap_or(0);
                    }
                    _ => survived = f.refilter_carried(p.cursor_name().map(OsString::as_os_str)),
                }
                // Without marks, what's selected is the item it was asked to select, if it's
                // here; nothing when it's gone (no other row is chosen in its place) or nothing
                // was selected; a folder newly opened, its first row.
                if !several {
                    f.none = p.none
                        || match (p.select.first(), &p.cursor) {
                            (Some(x), _) => f.cursor_name().as_ref() != Some(x),
                            (None, Some(_)) => true,
                            (None, None) => f.visible.is_empty(),
                        };
                } else if !survived {
                    // None of the items to select is here any more: nothing selected.
                    f.none = true;
                }
                f.anchor = f.sel;
                // The folder was read: its volume's free space, now that its identity says which
                // volume it's on (`volinfo.rs`).
                f.ask_free();
                if p.nav == Nav::Show {
                    // Where it was when it was hidden (clamped if the folder shrank since).
                    tx.scroll_to_instant(f.list_key(), 0.0, p.scroll);
                } else {
                    if moved {
                        tx.scroll_to_instant(f.list_key(), 0.0, 0.0);
                    }
                    f.reveal(tx);
                }
                // An inline rename of an item that's gone (removed from another window, or by
                // another app before a reload) ends: its field went with the row, and would
                // otherwise keep every key.
                if active
                    && let Some(r) = &self.rename
                    && !self.panes[ix].f.entries.iter().any(|e| e.os == r.os)
                {
                    self.rename = None;
                }
                if active {
                    if navigated {
                        self.note = None;
                    }
                    tx.set_title(title(&self.cur().cwd));
                    if p.rename {
                        self.rename_made();
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

    /// Folders First, on or off, for every tab of the window (and its closed tabs), each kept
    /// at its cursor.
    pub fn set_folders_first(&mut self, on: bool) {
        self.folders_first = on;
        for p in &mut self.panes {
            p.f.set_dirs_first(on);
            for m in p.tabs.iter_mut().flatten() {
                m.sort.dirs_first = on;
            }
        }
        for m in &mut self.closed {
            m.sort.dirs_first = on;
        }
    }

    /// The palette's Folders First: turned the other way, in this window, until it closes.
    pub fn toggle_folders_first(&mut self) {
        self.folders_set = true;
        let on = !self.folders_first;
        self.set_folders_first(on);
        self.note = Some(if on { "Folders first".into() } else { "Folders sorted with files".into() });
    }

    pub fn toggle_hidden(&mut self) {
        self.hidden_set = true;
        self.show_hidden = !self.show_hidden;
        let on = self.show_hidden;
        for p in &mut self.panes {
            p.f.set_hidden(on);
        }
    }

    /// Starts the inline rename of what New File or New Folder made, now that a reload has
    /// listed it: only when the selected row is that item, by name and identity. Nothing else
    /// is ever renamed in its place (it was moved away, or something else took its name).
    fn rename_made(&mut self) {
        let Some((path, id)) = self.made.take() else { return };
        let f = self.cur();
        let ours = f.selected().is_some_and(|e| f.cwd.join(&e.os) == path && (id.is_none() || e.id.is_none() || e.id == id));
        if ours {
            self.start_rename();
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
            PathKind::Original | PathKind::HiddenOriginal => {
                // Nothing on screen may hide it: a filter goes (a navigation clears it too),
                // and a hidden original turns hidden files on.
                if kind == PathKind::HiddenOriginal && !self.show_hidden {
                    self.toggle_hidden();
                    self.note = Some(format!(
                        "Showing hidden files: the original, “{}”, is hidden",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ));
                }
                let f = self.cur_mut();
                f.filter.clear();
                f.goto = false;
                match (path.parent(), path.file_name()) {
                    (Some(dir), Some(name)) if dir == self.cur().cwd && !self.cur().searching() => {
                        self.reload_selecting(vec![name.to_os_string()], false);
                    }
                    (Some(dir), Some(name)) => self.go(dir.to_path_buf(), Nav::New, Some(name.to_os_string())),
                    _ => self.go(path, Nav::New, None),
                }
            }
            PathKind::Broken => self.note = Some(format!("The original isn't there: {}", fs::pretty(&path))),
        }
    }

    /// Show Original: the item the cursor's symbolic link points at, selected in its folder in
    /// this pane (⌘[ comes back). The link is read on a worker, as a go-to's path is (it may be on
    /// a slow disk), waiting here at most a frame.
    pub fn show_original(&mut self) {
        let f = self.cur();
        let Some(link) = f.selected().filter(|e| e.is_link).map(|e| f.cwd.join(&e.os)) else { return };
        let id = crate::folder::next_request();
        self.going_to = Some(id);
        let wait = self.show_wait;
        let answer = crate::bounded::read_waiting(&self.path_check, wait, move || {
            let (path, kind) = original(&link);
            Some((id, path, kind))
        });
        if let Some(a) = answer {
            self.went_path(a);
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

    pub(crate) fn reload_settings_from(&mut self, path: Option<PathBuf>) {
        self.settings_loader.request(path, |p| p.map_or_else(Config::default, |p| Config::read(&p)));
    }

    /// A reloaded settings file. Keys apply at once; a preference applies only if the file
    /// changed it, so reloading doesn't undo ⌘. or a header click made since. A new sort
    /// applies to every tab.
    /// `say`: this is the window that reloaded them, which says so in its status bar.
    fn apply_settings(&mut self, c: Config, say: bool, tx: &mut Tx) {
        let (old, new) = (self.settings.clone(), c.settings.clone());
        if new.sort.dirs_first != old.sort.dirs_first {
            self.set_folders_first(new.sort.dirs_first);
        }
        if (new.sort.key, new.sort.desc) != (old.sort.key, old.sort.desc) {
            let sort = fs::Sort { dirs_first: self.folders_first, ..new.sort };
            for p in &mut self.panes {
                p.f.apply_sort(sort);
                for m in p.tabs.iter_mut().flatten() {
                    m.sort = sort;
                }
            }
        }
        if new.show_hidden != old.show_hidden {
            self.show_hidden = new.show_hidden;
            for p in &mut self.panes {
                p.f.set_hidden(new.show_hidden);
            }
        }
        // New tabs take the new default view; a changed icon size applies at once.
        if new.icon_size != old.icon_size {
            self.icon_size = new.icon_size;
        }
        // The settings' places are the default list: the user's own (sidebar.toml) stays.
        if new.places != old.places && !self.side.custom {
            self.places = places_for(&new);
            self.side.look_due = true;
        }
        self.apply_history_settings(&new);
        self.reveal(tx);
        if say {
            self.note = Some(c.summary().unwrap_or_else(|| "Settings reloaded".into()));
        }
        self.keymap = c.keymap;
        self.settings = c.settings;
    }
}

impl State {
    /// `jump.enabled` and `jump.exclude`, as the settings say.
    fn apply_history_settings(&mut self, s: &Settings) {
        let mut h = self.shared.history.borrow_mut();
        h.enabled = s.jump_enabled;
        h.exclude = crate::jump::exclude_globs(&s.jump_exclude);
    }

    /// The window is closing: every search walk and recheck stops where it is, and Batch
    /// Rename's folder read, before the last moment given to the Jump history's writer.
    pub fn stop_background(&mut self) {
        for p in &mut self.panes {
            p.f.tiles = None;
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
        let (keep, cursor) = if names.is_empty() { (p.select.clone(), p.cursor.clone()) } else { (names, None) };
        let scroll = p.scroll;
        f.request_chosen(f.cwd.clone(), Nav::Show, keep, cursor, false);
        if let Some(p) = &mut f.pending {
            p.scroll = scroll;
        }
        return;
    }
    // The selection as it is on screen, handed to the worker without copying a name; or the
    // items named (what an operation made).
    if names.is_empty() {
        f.request_kept(Nav::Reload, rename);
    } else {
        f.request(f.cwd.clone(), Nav::Reload, names, rename);
    }
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
