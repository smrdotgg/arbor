//! Arbor's windows: how each is made (the same callbacks for every window), the registry other
//! threads reach them through, and opening another one (⌘N, "Open in New Window").
//!
//! Every window is a real OS window in the one process and event loop, with its own `State`
//! (Rune's `open_window`). What they share is in `app::shared`. The registry is only for what
//! comes from outside the UI thread: requests from other apps, and the OS's word that volumes
//! changed, go to the window focused last ([`route`]).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use rune::{App, AppHandle, AppSpec, Chrome, Tx, WindowHandle};
#[cfg(not(test))]
use rune::{WindowOptions, WindowSpec};

use crate::app::State;

/// The window Arbor opens with. Windows opened later count up from 1.
pub const FIRST: u64 = 0;

/// The next window's id.
pub fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(FIRST + 1);
    NEXT.fetch_add(1, Relaxed)
}

/// An open window, as threads other than the UI's reach it.
struct Entry {
    id: u64,
    app: AppHandle<State>,
    /// Its native window, once created (to place a new window beside it).
    native: Option<WindowHandle>,
    /// When it last took focus (0: never).
    focus: u64,
}

static WINDOWS: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
static FOCUS: AtomicU64 = AtomicU64::new(1);

fn windows() -> std::sync::MutexGuard<'static, Vec<Entry>> {
    WINDOWS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Window `id` runs behind `app`.
pub fn register(id: u64, app: AppHandle<State>) {
    windows().push(Entry { id, app, native: None, focus: 0 });
}

/// Window `id` closed.
pub fn unregister(id: u64) {
    windows().retain(|e| e.id != id);
}

/// Window `id` took focus.
pub fn focused(id: u64) {
    let at = FOCUS.fetch_add(1, Relaxed);
    if let Some(e) = windows().iter_mut().find(|e| e.id == id) {
        e.focus = at;
    }
}

/// Which of `among` took focus last, if any did.
pub fn most_recent(among: &[u64]) -> Option<u64> {
    let w = windows();
    pick(w.iter().filter(|e| among.contains(&e.id)).map(|e| (e.id, e.focus)))
}

/// The window requests go to: the one focused last, or failing that the first opened.
fn pick(windows: impl Iterator<Item = (u64, u64)>) -> Option<u64> {
    let all: Vec<(u64, u64)> = windows.collect();
    all.iter().copied().max_by_key(|&(_, focus)| focus).filter(|&(_, f)| f > 0).or(all.first().copied()).map(|(id, _)| id)
}

/// Runs `f` in the window focused last (the first opened, before any took focus), from any
/// thread, and returns its handle so the caller can bring it forward. `None` with no window
/// open: the app is quitting.
pub fn route(f: impl FnOnce(&mut State, &mut Tx) + Send + 'static) -> Option<AppHandle<State>> {
    let w = windows();
    let id = pick(w.iter().map(|e| (e.id, e.focus)))?;
    let app = w.iter().find(|e| e.id == id).map(|e| e.app.clone())?;
    drop(w);
    app.update(f);
    Some(app)
}

/// Closes every window but `keep` (the bench's windows).
pub fn close_all_but(keep: u64) {
    let apps: Vec<AppHandle<State>> = windows().iter().filter(|e| e.id != keep).map(|e| e.app.clone()).collect();
    for app in apps {
        app.close();
    }
}

/// Window `id`'s native window.
#[cfg(not(test))]
fn native(id: u64) -> Option<WindowHandle> {
    windows().iter().find(|e| e.id == id).and_then(|e| e.native.clone())
}

fn set_native(id: u64, handle: &WindowHandle) {
    if let Some(e) = windows().iter_mut().find(|e| e.id == id) {
        e.native = Some(handle.clone());
    }
}

/// Every window's spec: its size, callbacks and view. `main` swaps in the timed callbacks for
/// the first window when timing is on.
pub fn spec() -> AppSpec<State> {
    AppSpec::<State> {
        min_size: Some((520.0, 320.0)),
        setup: Some(|s, waker| s.setup(waker)),
        poll: Some(|s, _, tx| s.poll(tx)),
        on_key: Some(|s, ev, tx| s.on_key(ev, tx)),
        ..AppSpec::new("Arbor", (980.0, 620.0), State::new, crate::view::view)
    }
}

/// What every window has beyond its spec: its frame, its close (the last window's lets the
/// user-data writers finish), its focus (for routing requests) and its first frame on screen.
pub fn configure(app: App<State>, id: u64) -> App<State> {
    // macOS: content runs under a transparent titlebar; the traffic lights sit in the sidebar's
    // top band and the path bar is the drag region. Other platforms keep their native frame.
    let chrome = Chrome { full_size_content: true, traffic_lights: Some((14.0, crate::view::BAR_H / 2.0 - 7.0)), drag_regions: true };
    // Searches stop first; then, when it's the last window, what's still queued for the
    // user-data writers (the Jump history, the sidebar) gets a moment to land.
    let app = app
        .chrome(chrome)
        .on_close(|s, _| closing(s))
        .on_focus(|s, on, _| {
            if on {
                focused(s.win);
                s.shared.focus(s.win);
            }
        })
        .on_window(move |h| set_native(id, h));
    // Work the window doesn't need for its first frame waits until that frame is on screen
    // (the sidebar's look at each favorite).
    let shown = app.handle();
    app.on_ready(move |_| shown.update(|s, _| s.first_frame_shown()))
}

/// A window's close (Rune hides it once this returns): its searches stop, it leaves the
/// others, and when it's the last, what's queued for the Jump history's and the sidebar's
/// writers gets its moment (`persist::DRAIN_WAIT` each: on macOS, Quit ends the process right
/// after this, RUNE-GAPS.md). It never waits for a file operation and writes no record: one cut
/// short goes on cleaning up, and is recorded, off this thread (`leftovers.rs`).
pub(crate) fn closing(s: &mut State) {
    s.stop_background();
    if s.leave() {
        crate::persist::drain_at_exit();
    }
}

/// Opens `state` (made by `State::new_window`) in a window of its own, beside the window
/// `from` and the same size, at the end of this turn of the event loop.
pub fn open(state: State, from: u64) {
    #[cfg(test)]
    {
        let _ = from;
        tests::OPENED.with(|o| o.borrow_mut().push(state));
    }
    #[cfg(not(test))]
    {
        // Rune opens no window in a headless run (a snapshot, a script): nothing to join.
        if rune::is_headless() || rune::tooling::headless_run().is_some() {
            let mut state = state;
            state.leave();
            return;
        }
        let id = state.win;
        let mut options = WindowOptions::default();
        // A little down and right of the window it came from (Wayland doesn't say where windows
        // are: there the compositor places it).
        if let Some(p) = native(from).and_then(|h| h.placement()) {
            let step = (28.0 * p.scale).round() as i32;
            options.position = Some((p.position.0 + step, p.position.1 + step));
            if !p.maximized && !p.full_screen {
                options.physical_size = Some(p.content_size);
            }
        }
        // Timed (`ARBOR_TIMING`, the bench): when its folder shows.
        let timed = crate::timing::on();
        let mut spec = spec();
        if timed {
            crate::timing::window_opening(&state);
            spec.poll = Some(crate::timing::window_poll);
            spec.setup = Some(crate::timing::window_setup);
        }
        let mut app = configure(App::new(spec), id).init(move || state);
        if timed {
            app = app.on_frame_encoded(crate::timing::window_frame);
        }
        register(id, app.handle());
        rune::open_window(WindowSpec::from_app(format!("arbor-{id}"), app).options(options));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    thread_local! {
        /// The windows this thread's tests opened, newest last, instead of on screen.
        pub(crate) static OPENED: std::cell::RefCell<Vec<State>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// The window the last ⌘N or "Open in New Window" opened.
    pub(crate) fn take_opened() -> Option<State> {
        OPENED.with(|o| o.borrow_mut().pop())
    }

    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use rune::Tx;

    use crate::app::tests::{finish_job, settle_all, state_in};
    use crate::commands::Cmd;

    fn folder(name: &str, dirs: &[&str]) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-windows-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for sub in dirs {
            std::fs::create_dir_all(d.join(sub)).unwrap();
        }
        std::fs::write(d.join("file.txt"), "x").unwrap();
        d
    }

    /// A first window on `d`, and a second opened from it with ⌘N, both settled.
    fn two(d: &Path) -> (State, State) {
        let mut a = state_in(d);
        settle_all(&mut a);
        a.run(Cmd::NewWindow, &mut Tx::default());
        let mut b = take_opened().expect("⌘N opened a window");
        settle_all(&mut b);
        // Its first poll names it.
        assert!(b.poll(&mut Tx::default()), "a new window polls its title");
        (a, b)
    }

    fn names(s: &State) -> Vec<String> {
        let f = s.cur();
        f.visible.iter().map(|&e| f.entries[e as usize].name.clone()).collect()
    }

    /// Polls `s` as frames would until `done` (or 10 s pass).
    fn until(s: &mut State, what: &str, done: impl Fn(&State) -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !done(s) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(2));
            s.poll(&mut Tx::default());
        }
        assert!(done(s), "{what}");
    }

    fn select(s: &mut State, name: &str) {
        let i = names(s).iter().position(|n| n == name).unwrap();
        s.cur_mut().select(i);
    }

    #[test]
    fn requests_go_to_the_window_focused_last_or_the_first() {
        assert_eq!(pick([].into_iter()), None);
        assert_eq!(pick([(0, 0), (3, 0)].into_iter()), Some(0), "none focused yet: the first opened");
        assert_eq!(pick([(0, 5), (3, 9), (4, 2)].into_iter()), Some(3));
        assert_eq!(pick([(0, 0), (3, 4)].into_iter()), Some(3));
    }

    /// The registry follows focus: a request from another app goes to the window that took it
    /// last, and a closed window is never picked.
    #[test]
    fn the_registry_follows_focus_and_closes() {
        let app = || App::new(spec());
        // Ids no other test's windows use (the registry is the process's).
        let (x, y) = (u64::MAX - 2, u64::MAX - 1);
        register(x, app().handle());
        register(y, app().handle());
        assert_eq!(most_recent(&[x, y]), Some(x), "before any focus: the first opened");
        focused(y);
        assert_eq!(most_recent(&[x, y]), Some(y));
        focused(x);
        assert_eq!(most_recent(&[x, y]), Some(x));
        unregister(x);
        assert_eq!(most_recent(&[x, y]), Some(y));
        unregister(y);
        assert_eq!(most_recent(&[x, y]), None);
    }

    /// ⌘N: a second window on the folder on screen, sharing the first's settings and favorites,
    /// with its own tabs, selection, history and view; neither changes the other.
    #[test]
    fn two_windows_have_their_own_tabs_selection_and_history() {
        let d = folder("own", &["a", "b"]);
        let (mut a, mut b) = two(&d);
        assert_ne!(a.win, b.win);
        assert_eq!(b.cur().cwd, d, "the folder on screen");
        assert_eq!(names(&b), names(&a), "read, and shown at once");
        assert_eq!(b.places, a.places);
        assert_eq!(a.windows(), 2);
        assert!(std::rc::Rc::ptr_eq(&a.shared, &b.shared));
        // Navigation, tabs, selection and view are each window's.
        b.go(d.join("a"), crate::folder::Nav::New, None);
        settle_all(&mut b);
        b.run(Cmd::NewTab, &mut Tx::default());
        b.run(Cmd::AsIcons, &mut Tx::default());
        select(&mut a, "file.txt");
        assert_eq!(a.cur().cwd, d);
        assert_eq!(a.panes[0].tabs.len(), 1);
        assert_eq!(b.panes[0].tabs.len(), 2);
        assert!(a.cur().back.is_empty(), "the first window went nowhere");
        assert_eq!(b.cur().view, crate::grid::ViewMode::Icons);
        assert_eq!(a.cur().view, crate::grid::ViewMode::List);
        b.run(Cmd::PrevTab, &mut Tx::default());
        settle_all(&mut b);
        assert_eq!(b.cur().back, std::slice::from_ref(&d), "its own history");
        b.run(Cmd::Back, &mut Tx::default());
        settle_all(&mut b);
        assert_eq!(b.cur().cwd, d);
        assert_eq!(a.selected().map(|e| e.name.as_str()), Some("file.txt"));
        // Nothing went from one to the other.
        assert!(!a.poll(&mut Tx::default()) && !b.poll(&mut Tx::default()), "no messages between windows that only navigated");
        // The Jump history is one: a visit in either is there for both.
        assert!(a.shared.history.borrow().enabled);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// "Open in New Window" opens the one folder selected; anything else says what it needs.
    #[test]
    fn open_in_new_window_opens_the_selected_folder() {
        let d = folder("open-folder", &["a"]);
        let mut a = state_in(&d);
        settle_all(&mut a);
        select(&mut a, "file.txt");
        assert_eq!(a.unavailable(Cmd::OpenInNewWindow), Some("Select one folder first"));
        select(&mut a, "a");
        assert!(a.menu_items(true).iter().flatten().any(|(act, _, on)| *on && act.cmd() == Some(Cmd::OpenInNewWindow)));
        a.run(Cmd::OpenInNewWindow, &mut Tx::default());
        let mut b = take_opened().expect("opened");
        settle_all(&mut b);
        assert_eq!(b.cur().cwd, d.join("a"));
        assert!(b.cur().back.is_empty(), "a fresh history");
        assert_eq!(a.cur().cwd, d, "the first window stays where it was");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// One file-operation slot and one undo stack for the app: while the first window's
    /// operation runs, the second can't start one (and says it's another window's); once it
    /// lands, the second window, on the same folder, reads it again; its ⌘Z undoes the first
    /// window's operation, and the first window refreshes in turn.
    #[test]
    fn one_operation_slot_and_one_undo_stack_across_windows() {
        let d = folder("slot", &[]);
        let (mut a, mut b) = two(&d);
        a.new_folder();
        assert!(a.owns_job());
        assert_eq!(b.unavailable(Cmd::NewFolder), Some(crate::app::BUSY_ELSEWHERE));
        assert_eq!(b.unavailable(Cmd::Undo), Some(crate::app::BUSY_ELSEWHERE));
        assert!(b.unavailable(Cmd::Cancel).is_some(), "only the window running it cancels it");
        b.new_folder();
        assert!(!b.owns_job(), "one at a time");
        assert_eq!(b.note.as_deref(), Some(crate::app::BUSY_ELSEWHERE));
        finish_job(&mut a);
        a.rename = None;
        assert!(names(&a).contains(&"untitled folder".to_string()));
        // The second window, showing the same folder, reads it again.
        until(&mut b, "the other window refreshed", |b| names(b).contains(&"untitled folder".to_string()));
        assert!(b.rename.is_none(), "the rename is the first window's");
        // Undo from the second window.
        assert_eq!(b.unavailable(Cmd::Undo), None);
        b.run(Cmd::Undo, &mut Tx::default());
        assert!(b.owns_job() && a.busy());
        finish_job(&mut b);
        assert!(!names(&b).contains(&"untitled folder".to_string()));
        until(&mut a, "the first window refreshed after the undo", |a| !names(a).contains(&"untitled folder".to_string()));
        assert_eq!(a.shared.redo.borrow().len(), 1, "one redo stack");
        assert_eq!(a.unavailable(Cmd::Redo), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A file-operation key in a window while another window's operation runs is taken and
    /// says why (review: it went unhandled, with no note).
    #[test]
    fn a_busy_key_in_another_window_says_why() {
        let d = folder("busy-key", &[]);
        let (mut a, mut b) = two(&d);
        a.new_folder();
        b.note = None;
        let new_folder = rune::KeyEvent { key: rune::Key::Command('n'), mods: rune::Mods { cmd: true, shift: true, ..Default::default() } };
        assert!(b.on_key(&new_folder, &mut Tx::default()), "taken");
        assert!(!b.owns_job());
        assert_eq!(b.note.as_deref(), Some(crate::app::BUSY_ELSEWHERE));
        // And in the window running it, the same, as "Another file operation is running".
        a.note = None;
        assert!(a.on_key(&new_folder, &mut Tx::default()));
        assert_eq!(a.note.as_deref(), Some(crate::app::BUSY));
        finish_job(&mut a);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// An operation's note goes from the window that says it when another window undoes it
    /// (real-Mac QA: after an undo in window 1, window 2 still said "Moved 1 item").
    #[test]
    fn undoing_an_operation_clears_its_note_in_the_window_that_did_it() {
        let d = folder("stale-note", &["into"]);
        let (mut a, mut b) = two(&d);
        b.go(d.join("into"), crate::folder::Nav::New, None);
        settle_all(&mut b);
        select(&mut a, "file.txt");
        a.run(Cmd::Cut, &mut Tx::default());
        let files = a.shared.clip.borrow().as_ref().unwrap().paths.clone();
        b.paste(&rune::ClipboardContent::new("").with_files(files));
        finish_job(&mut b);
        assert_eq!(b.note.as_deref(), Some("Moved 1 item"));
        until(&mut a, "refreshed", |a| !names(a).contains(&"file.txt".to_string()));
        a.run(Cmd::Undo, &mut Tx::default());
        finish_job(&mut a);
        assert!(d.join("file.txt").exists());
        until(&mut b, "the moved note went", |b| b.note.is_none());
        // A note said since stays: only the undone outcome's goes.
        b.note = Some("something else".into());
        a.run(Cmd::Redo, &mut Tx::default());
        finish_job(&mut a);
        b.poll(&mut Tx::default());
        assert_eq!(b.note.as_deref(), Some("something else"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// New File renames the file it made in the window that asked, and only there: another
    /// window on the folder shows it, unrenamed; a window that landed it because the asking
    /// window closed doesn't rename it either.
    #[test]
    fn new_file_renames_only_in_the_window_that_asked() {
        let d = folder("new-file", &[]);
        let (mut a, mut b) = two(&d);
        a.run(Cmd::NewFile, &mut Tx::default());
        finish_job(&mut a);
        assert_eq!(a.rename.as_ref().map(|r| r.os.clone()), Some("untitled.txt".into()));
        until(&mut b, "the other window shows it", |b| names(b).contains(&"untitled.txt".to_string()));
        assert!(b.rename.is_none(), "not renamed in the other window");
        // The asking window closes before it lands: the window that lands it doesn't rename.
        a.rename = None;
        a.run(Cmd::NewFile, &mut Tx::default());
        assert!(!a.leave());
        until(&mut b, "landed by the window left", |b| !b.busy() && names(b).contains(&"untitled 2.txt".to_string()));
        assert!(b.rename.is_none(), "no rename in a window that didn't ask");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Make Symbolic Link from one window: the other on the folder shows the link, and an undo
    /// from the other clears the first's note.
    #[cfg(unix)]
    #[test]
    fn a_link_made_in_one_window_shows_and_undoes_in_the_other() {
        let d = folder("links", &[]);
        let (mut a, mut b) = two(&d);
        select(&mut a, "file.txt");
        a.run(Cmd::MakeLink, &mut Tx::default());
        finish_job(&mut a);
        assert!(a.note.is_some());
        until(&mut b, "the link shows in the other window", |b| names(b).contains(&"file link.txt".to_string()));
        b.run(Cmd::Undo, &mut Tx::default());
        finish_job(&mut b);
        assert!(!d.join("file link.txt").exists(), "undone");
        until(&mut a, "the link went and so did its note", |a| a.note.is_none() && !names(a).contains(&"file link.txt".to_string()));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Esc ends the inline rename of the window it's pressed in, not another window's.
    #[test]
    fn esc_cancels_only_its_own_windows_rename() {
        let d = folder("rename-esc", &["sub"]);
        let (mut a, mut b) = two(&d);
        select(&mut a, "file.txt");
        a.start_rename();
        select(&mut b, "sub");
        b.start_rename();
        let esc = rune::KeyEvent { key: rune::Key::Escape, mods: rune::Mods::default() };
        assert!(b.on_key(&esc, &mut Tx::default()));
        assert!(b.rename.is_none(), "one Esc");
        assert_eq!(a.rename.as_ref().map(|r| r.os.clone()), Some("file.txt".into()), "the other window's rename stays");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A window on another folder isn't read again; one whose hidden tab shows the folder reads
    /// it when it's shown, as panes do.
    #[test]
    fn only_windows_showing_a_changed_folder_read_it() {
        let d = folder("affected", &["a"]);
        let (mut a, mut b) = two(&d);
        b.go(d.join("a"), crate::folder::Nav::New, None);
        settle_all(&mut b);
        a.new_folder();
        finish_job(&mut a);
        let lands = b.lands;
        assert!(b.poll(&mut Tx::default()), "told");
        assert!(!b.is_loading() && b.lands == lands, "nothing read: it shows another folder");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The clipboard's files are one: ⌘X in one window, ⌘V in the other moves.
    #[test]
    fn a_cut_in_one_window_moves_when_pasted_in_another() {
        let d = folder("cut", &["into"]);
        let (mut a, mut b) = two(&d);
        b.go(d.join("into"), crate::folder::Nav::New, None);
        settle_all(&mut b);
        select(&mut a, "file.txt");
        a.run(Cmd::Cut, &mut Tx::default());
        let files = a.shared.clip.borrow().as_ref().unwrap().paths.clone();
        b.paste(&rune::ClipboardContent::new("").with_files(files));
        finish_job(&mut b);
        assert!(d.join("into/file.txt").exists() && !d.join("file.txt").exists(), "moved");
        until(&mut a, "the first window refreshed", |a| !names(a).contains(&"file.txt".to_string()));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// An inline rename in one window of an item another window trashes ends when the folder
    /// is read again: its field went with the row, and would otherwise keep every key.
    #[test]
    fn a_rename_of_an_item_another_window_removed_ends() {
        let d = folder("rename-gone", &[]);
        let (mut a, mut b) = two(&d);
        select(&mut b, "file.txt");
        b.start_rename();
        select(&mut a, "file.txt");
        a.trash_selected();
        finish_job(&mut a);
        until(&mut b, "the rename ended with its row", |b| b.rename.is_none());
        assert!(!names(&b).contains(&"file.txt".to_string()));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A window that closes while its operation runs hands it to another, which lands it, its
    /// note and its undo.
    #[test]
    fn a_closing_window_hands_its_operation_on() {
        let d = folder("handover", &[]);
        let (mut a, mut b) = two(&d);
        a.new_folder();
        assert!(!a.leave(), "not the last window");
        assert!(!a.owns_job());
        assert!(b.busy(), "still running, now the second window's");
        until(&mut b, "landed in the window that took it over", |b| !b.owns_job() && !b.busy());
        assert_eq!(b.shared.undo.borrow().len(), 1);
        assert_eq!(b.windows(), 1);
        assert!(b.leave(), "the last window");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The operation is the app's, not a window's: when its owner closes and the window made
    /// its owner closes too before it ever polled, the window left lands it (review: it was
    /// handed through inboxes and dropped with the second window, leaving the app busy forever
    /// and the undo lost).
    #[test]
    fn an_operation_survives_its_owners_closing_one_after_another() {
        let d = folder("handover-twice", &[]);
        let (mut a, mut b) = two(&d);
        let mut c = b.new_window(d.clone());
        settle_all(&mut c);
        c.poll(&mut Tx::default());
        a.new_folder();
        assert!(!a.leave());
        assert!(c.owns_job() || b.owns_job(), "another window owns it at once");
        assert!(!c.leave());
        until(&mut b, "landed in the window left", |b| !b.busy());
        assert_eq!(b.shared.undo.borrow().len(), 1, "its undo kept");
        assert!(d.join("untitled folder").is_dir());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A copy into `into` of a 20-file folder, stopped right after making `02.txt` (the worker
    /// waits at a gate), cleanup's removal of that file meeting `unmake`; the record goes in the
    /// test's folder and the cleanup gets 300 ms.
    fn stalled_copy(tag: &str, unmake: crate::ops::gate::Unmake) -> (PathBuf, State, std::sync::mpsc::Sender<()>) {
        let d = folder(tag, &["payload", "into"]);
        for i in 1..=20 {
            std::fs::write(d.join(format!("payload/{i:02}.txt")), "x").unwrap();
        }
        let mut a = state_in(&d);
        settle_all(&mut a);
        a.quit_record = Some(d.join("leftovers.txt"));
        a.quit_wait = Duration::from_millis(300);
        let made = d.join("into/payload/02.txt");
        let (reached_tx, reached) = std::sync::mpsc::channel();
        let (go, go_rx) = std::sync::mpsc::channel();
        crate::ops::gate::MADE.lock().unwrap().push((made.clone(), reached_tx, go_rx));
        crate::ops::gate::UNMAKE.lock().unwrap().push((made, unmake));
        a.start(crate::ops::Op::Copy { sources: vec![d.join("payload")], dest: d.join("into") });
        reached.recv_timeout(Duration::from_secs(10)).expect("the copy reached 02.txt");
        (d, a, go)
    }

    /// The record at `d`, once `done` holds of it (or 10 s pass).
    fn record(d: &Path, done: impl Fn(&[crate::leftovers::Leftover]) -> bool) -> Vec<crate::leftovers::Leftover> {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let got = crate::leftovers::read(&d.join("leftovers.txt"));
            if done(&got) || Instant::now() > end {
                return got;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Closing the last window mid-copy returns at once, even with the cleanup stalled: the
    /// copy is cancelled and its cleanup goes on off the UI thread (review: the close callback
    /// waited up to 2 s with the window still on screen). Its time running out is recorded:
    /// the copy left partly there, and why.
    #[test]
    fn closing_the_last_window_never_waits_for_a_stalled_cleanup() {
        let (stall, stall_rx) = std::sync::mpsc::channel();
        let (d, mut a, go) = stalled_copy("quit-stall", crate::ops::gate::Unmake::Stall(stall_rx));
        let progress = a.shared.job.borrow().as_ref().unwrap().run.progress.clone();
        let t = Instant::now();
        closing(&mut a);
        let took = t.elapsed();
        assert!(took < Duration::from_millis(50), "the close callback took {took:?}");
        assert!(progress.cancel.load(std::sync::atomic::Ordering::SeqCst), "cancelled at the close");
        assert!(!a.busy());
        let partly = d.join("into/payload");
        // Before the cleanup could run (the copy still waits at its gate): the copy being made.
        let got = record(&d, |g| g.iter().any(|l| l.path == partly));
        assert!(got.iter().any(|l| l.path == partly), "provisionally at once: {got:?}");
        go.send(()).unwrap(); // the copy goes on, sees the cancel, and stalls removing 02.txt
        std::thread::sleep(Duration::from_millis(500)); // past the 300 ms the cleanup gets
        let got = record(&d, |g| g.iter().any(|l| l.path == partly && l.why == crate::leftovers::UNFINISHED));
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!((&got[0].path, got[0].why.as_str()), (&partly, crate::leftovers::UNFINISHED));
        drop(stall);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A cleanup that finishes but can't remove something (permission denied) records that
    /// item and why (review: the outcome was dropped, and the process exited saying nothing).
    #[test]
    fn a_cleanup_that_cant_remove_something_at_quit_records_it() {
        let (d, mut a, go) = stalled_copy("quit-refused", crate::ops::gate::Unmake::Refuse);
        closing(&mut a);
        go.send(()).unwrap();
        let kept = d.join("into/payload/02.txt");
        let got = record(&d, |g| g.iter().any(|l| l.path == kept));
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].path, kept);
        assert!(got[0].why.starts_with("it couldn't be removed"), "{}", got[0].why);
        assert!(!d.join("into/payload/01.txt").exists(), "the rest went");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The record is written by the quit thread, never by the close callback or a writer the
    /// callback flushes (review: the callback's exit drain flushed a record writer, I/O in the
    /// close callback).
    #[test]
    fn the_quit_thread_writes_the_record() {
        let (d, mut a, go) = stalled_copy("quit-thread", crate::ops::gate::Unmake::Refuse);
        let file = d.join("leftovers.txt");
        closing(&mut a);
        go.send(()).unwrap();
        let kept = d.join("into/payload/02.txt");
        record(&d, |g| g.iter().any(|l| l.path == kept));
        let writers: Vec<String> =
            crate::leftovers::WRITES.lock().unwrap().iter().filter(|(f, _)| *f == file).map(|(_, t)| t.clone()).collect();
        assert!(!writers.is_empty());
        assert!(writers.iter().all(|t| t == "arbor-quit"), "{writers:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The record names only what the copy confirmed it made, as it made it: a file another
    /// app put in its place afterwards is never offered as Arbor's leftover (review: the
    /// planned destination was recorded, and a user's own file then named as left by Arbor).
    #[test]
    fn a_leftover_replaced_since_isnt_offered() {
        let d = folder("quit-replaced", &["src", "dst"]);
        std::fs::write(d.join("src/original.txt"), "original").unwrap();
        let target = d.join("dst/original.txt");
        let mut a = state_in(&d);
        settle_all(&mut a);
        a.quit_record = Some(d.join("leftovers.txt"));
        a.quit_wait = Duration::from_millis(100);
        let (reached_tx, reached) = std::sync::mpsc::channel();
        let (go, go_rx) = std::sync::mpsc::channel();
        crate::ops::gate::MADE.lock().unwrap().push((target.clone(), reached_tx, go_rx));
        a.start(crate::ops::Op::Copy { sources: vec![d.join("src/original.txt")], dest: d.join("dst") });
        reached.recv_timeout(Duration::from_secs(10)).unwrap();
        std::fs::rename(&target, d.join("saved-copy")).unwrap();
        std::fs::write(&target, "the user's own data").unwrap();
        closing(&mut a);
        // The copy stays stuck past the 100 ms: the record is what the copy knew it made.
        let got = record(&d, |g| !g.is_empty());
        assert_eq!(got.iter().map(|l| &l.path).collect::<Vec<_>>(), [&target], "recorded as made");
        let left = crate::leftovers::take(&d.join("leftovers.txt"));
        assert!(left.is_empty(), "not offered: it isn't what Arbor made {left:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "the user's own data");
        drop(go);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Quitting ends on time even when the disk under the record never answers: `main`'s wait
    /// for the quit thread stops at the cleanup's deadline plus a fixed margin (review: it
    /// joined the thread, and a stuck disk kept the process alive).
    #[test]
    fn quitting_ends_on_time_when_the_record_cant_be_written() {
        let (d, mut a, go) = stalled_copy("quit-stuck-disk", crate::ops::gate::Unmake::Refuse);
        a.quit_wait = Duration::from_millis(100);
        let (stuck, stuck_rx) = std::sync::mpsc::channel();
        crate::leftovers::STALL.lock().unwrap().push((d.join("leftovers.txt"), stuck_rx));
        closing(&mut a);
        let t = Instant::now();
        let (ended, ended_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            crate::leftovers::finish();
            let _ = ended.send(());
        });
        assert!(ended_rx.recv_timeout(Duration::from_secs(3)).is_ok(), "main's wait never ended");
        // Other tests' quit threads may be waited on too: each is bounded the same way.
        assert!(t.elapsed() < Duration::from_millis(1500), "{:?}", t.elapsed());
        drop(stuck);
        drop(go);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A file stalled mid-copy when the last window closes (created, one byte of 8,192 written)
    /// is recorded and offered at the next launch: it's registered the moment it's created, by
    /// the identity read off the copy's handle (review: it was registered only once copied, so
    /// a file cut short mid-copy was left without a word).
    #[cfg(unix)]
    #[test]
    fn a_file_stalled_mid_copy_at_quit_is_recorded() {
        let d = folder("quit-mid-file", &["src", "dst"]);
        std::fs::write(d.join("src/big.bin"), vec![7u8; 8192]).unwrap();
        let target = d.join("dst/big.bin");
        let mut a = state_in(&d);
        settle_all(&mut a);
        a.quit_record = Some(d.join("leftovers.txt"));
        a.quit_wait = Duration::from_millis(100);
        let (reached_tx, reached) = std::sync::mpsc::channel();
        let (go, go_rx) = std::sync::mpsc::channel();
        crate::noclobber::gate::FIRST_BYTE.lock().unwrap().push((target.clone(), reached_tx, go_rx));
        a.start(crate::ops::Op::Copy { sources: vec![d.join("src/big.bin")], dest: d.join("dst") });
        reached.recv_timeout(Duration::from_secs(10)).expect("created, first byte written");
        assert_eq!(std::fs::metadata(&target).unwrap().len(), 1);
        closing(&mut a);
        let got = record(&d, |g| !g.is_empty());
        assert_eq!(got.iter().map(|l| &l.path).collect::<Vec<_>>(), [&target], "{got:?}");
        assert_eq!(got[0].why, crate::leftovers::UNFINISHED);
        // The next launch offers it: it's still the file Arbor made, though half written.
        let left = crate::leftovers::take(&d.join("leftovers.txt"));
        assert_eq!(left.iter().map(|l| &l.path).collect::<Vec<_>>(), [&target]);
        drop(go);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A cancelled copy whose cleanup went well records nothing.
    #[test]
    fn a_clean_cancel_at_quit_records_nothing() {
        let d = folder("quit-clean", &["payload", "into"]);
        for i in 1..=20 {
            std::fs::write(d.join(format!("payload/{i:02}.txt")), "x").unwrap();
        }
        let mut a = state_in(&d);
        settle_all(&mut a);
        a.quit_record = Some(d.join("leftovers.txt"));
        let made = d.join("into/payload/02.txt");
        let (reached_tx, reached) = std::sync::mpsc::channel();
        let (go, go_rx) = std::sync::mpsc::channel();
        crate::ops::gate::MADE.lock().unwrap().push((made, reached_tx, go_rx));
        a.start(crate::ops::Op::Copy { sources: vec![d.join("payload")], dest: d.join("into") });
        reached.recv_timeout(Duration::from_secs(10)).unwrap();
        closing(&mut a);
        go.send(()).unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        while d.join("into/payload").exists() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!d.join("into/payload").exists(), "cleaned up");
        let got = record(&d, |g| g.is_empty());
        assert!(got.is_empty(), "{got:?}");
        assert!(!d.join("leftovers.txt").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A window made the operation's owner before Rune set it up gets the worker's wakes once
    /// it is (review: only a later poll would have found the outcome).
    #[test]
    fn an_owner_set_up_late_gets_the_operations_wakes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let d = folder("late-setup", &[]);
        let mut a = state_in(&d);
        settle_all(&mut a);
        a.setup(Arc::new(|| {}));
        let mut b = a.new_window(d.clone());
        a.new_folder();
        a.leave();
        assert!(b.owns_job());
        let wakes = Arc::new(AtomicUsize::new(0));
        let n = wakes.clone();
        b.setup(Arc::new(move || {
            n.fetch_add(1, SeqCst);
        }));
        wakes.store(0, SeqCst);
        b.shared.op_wake()();
        assert_eq!(wakes.load(SeqCst), 1, "the worker's wake reaches it");
        finish_job(&mut b);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// ⌘W on a window's last tab closes the window while another is open; the very last tab of
    /// the very last window stays, as before.
    #[test]
    fn close_tab_on_the_last_tab_closes_the_window_only_when_others_are_open() {
        let d = folder("close", &[]);
        let mut a = state_in(&d);
        settle_all(&mut a);
        assert_eq!(a.unavailable(Cmd::CloseTab), Some("This is the only tab"));
        a.run(Cmd::NewWindow, &mut Tx::default());
        let mut b = take_opened().unwrap();
        assert_eq!(a.unavailable(Cmd::CloseTab), None);
        assert_eq!(b.unavailable(Cmd::CloseTab), None);
        // A window with two tabs closes a tab, not itself.
        b.run(Cmd::NewTab, &mut Tx::default());
        b.run(Cmd::CloseTab, &mut Tx::default());
        assert_eq!(b.panes[0].tabs.len(), 1);
        assert!(!b.leave(), "the first window is still open");
        assert_eq!(a.unavailable(Cmd::CloseTab), Some("This is the only tab"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Reload Settings in one window applies in every window; only the one that asked says so.
    #[test]
    fn settings_reloaded_in_one_window_apply_in_all() {
        let d = folder("settings", &[]);
        let (mut a, mut b) = two(&d);
        let file = d.join("config.toml");
        std::fs::write(&file, "[general]\nshow_hidden = true\n").unwrap();
        a.reload_settings_from(Some(file));
        until(&mut a, "reloaded", |a| a.show_hidden);
        assert!(a.note.is_some());
        b.note = None;
        until(&mut b, "the other window took them", |b| b.show_hidden);
        assert_eq!(b.note, None, "said in the window that reloaded");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A favorite added in one window shows in the other.
    #[test]
    fn favorites_changed_in_one_window_show_in_all() {
        let d = folder("favorites", &["fav"]);
        let (mut a, mut b) = two(&d);
        a.add_favorites(vec![d.join("fav")], None);
        until(&mut b, "the other window shows it", |b| b.places.iter().any(|(_, p)| *p == d.join("fav")));
        assert_eq!(a.places, b.places);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Jump's history is one: a visit in the second window ranks in the first's Jump.
    #[test]
    fn jump_history_is_shared() {
        let d = folder("jump", &["somewhere-unique"]);
        let (mut a, mut b) = two(&d);
        b.go(d.join("somewhere-unique"), crate::folder::Nav::New, None);
        settle_all(&mut b);
        a.open_jump(&mut Tx::default());
        until(&mut a, "the history landed", |a| a.jump.as_ref().is_some_and(|j| !j.loading));
        let agg = a.shared.history.borrow();
        assert!(agg.agg.as_ref().unwrap().get(&d.join("somewhere-unique")).is_some());
        drop(agg);
        let _ = std::fs::remove_dir_all(&d);
    }
}
