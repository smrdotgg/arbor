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
    Start {
        config: Config { settings, ..Config::default() },
        start: dir,
        places: Vec::new(),
        note: None,
        custom: false,
        leftovers: None,
        cols: Default::default(),
    }
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
    while (s.panes.iter().any(|p| p.f.is_loading()) || s.going_to.is_some()) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(2));
        s.poll(&mut Tx::default());
    }
    assert!(!s.panes.iter().any(|p| p.f.is_loading()), "never landed");
}

pub(crate) fn state_in(d: &Path) -> State {
    let dir = d.to_path_buf();
    State::boot(Booting::spawn(Duration::ZERO, move || settings_for(dir, Settings::default())), Duration::from_secs(5))
}

/// A drag out of the list that nothing took (as a drop on a folder row in the same window
/// ends on macOS) says how to move files instead of doing nothing silently; one that moved
/// files reads the folder again; a copy changes nothing here.
#[test]
fn a_drag_nothing_took_says_how_to_move_files() {
    let d = folder("drag-end", 2);
    let mut s = state_in(&d);
    s.drag_out_ended(rune::DragOutcome::Copied);
    assert_eq!(s.note, None);
    s.drag_out_ended(rune::DragOutcome::Cancelled);
    assert_eq!(s.note.as_deref(), Some(crate::actions::DRAG_NOT_TAKEN));
    s.drag_out_ended(rune::DragOutcome::Moved);
    assert!(s.is_loading());
    settle_all(&mut s);
}

/// The QA's "can't copy a folder into itself": New Folder asks for a reload that selects the
/// new folder; the user clicks a file before it lands, then ⌘C. The landing used to put the
/// selection back on the new folder, so ⌘C copied it and a paste into it failed. The click
/// now stands, and ⌘C copies the file.
#[test]
fn a_reload_keeps_a_selection_made_while_it_was_on_its_way() {
    let d = folder("reload-keeps-pick", 0);
    std::fs::write(d.join("delete-me-1.txt"), "x").unwrap();
    std::fs::write(d.join("other.txt"), "x").unwrap();
    let mut s = state_in(&d);
    settle_all(&mut s);
    s.new_folder();
    assert!(s.owns_job(), "made on the worker, not on the window's thread");
    wait_job(&mut s);
    assert!(s.is_loading(), "the reload is on its way");
    let i = s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].name == "delete-me-1.txt").unwrap();
    s.cur_mut().select(i);
    settle_all(&mut s);
    assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some("delete-me-1.txt"));
    assert!(s.rename.is_none(), "no rename started on a folder the user moved away from");
    s.copy_selected(false, &mut Tx::default());
    assert_eq!(s.shared.clip.borrow().as_ref().map(|c| c.paths.clone()), Some(vec![d.join("delete-me-1.txt")]));
    // With nothing picked meanwhile, the reload still selects the new folder, renaming it.
    s.new_folder();
    finish_job(&mut s);
    assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some("untitled folder 2"));
    assert!(s.rename.is_some());
}

/// The review's case: a reload on its way, Select All, then ⌘-click the cursor's item off.
/// The landing used to select it again, so the next trash, cut or move took it too.
#[test]
fn a_reload_never_selects_an_item_the_user_deselected() {
    let d = folder("reload-deselect", 3);
    let mut s = state_in(&d);
    settle_all(&mut s);
    s.reload();
    s.cur_mut().select_all();
    let cursor = s.cur().sel;
    s.cur_mut().toggle_mark(cursor);
    let want = s.cur().target_paths();
    assert_eq!(want.len(), 2);
    settle_all(&mut s);
    assert_eq!(s.cur().target_paths(), want, "the deselected item isn't a target");
    // One left selected, the cursor on one deselected: still exactly that one.
    s.reload();
    let f = s.cur_mut();
    f.select(0);
    f.toggle_mark(1);
    f.toggle_mark(1);
    let want = s.cur().target_paths();
    assert_eq!(want, [d.join("f0")]);
    settle_all(&mut s);
    assert_eq!(s.cur().target_paths(), want);
}

/// Lands the file operation once it's done (or 10 s pass), without letting any listing land:
/// what a frame's `poll_job` does, alone.
pub(crate) fn wait_job(s: &mut State) {
    let until = Instant::now() + Duration::from_secs(10);
    while s.owns_job() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
        s.poll_job();
    }
    assert!(!s.owns_job(), "the operation never finished");
}

/// Polls as frames would until the file operation lands (or 10 s pass).
pub(crate) fn finish_job(s: &mut State) {
    let until = Instant::now() + Duration::from_secs(10);
    while s.owns_job() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(2));
        s.poll(&mut Tx::default());
    }
    assert!(!s.owns_job(), "the operation never finished");
    settle_all(s);
}

/// ⇧⌘Z redoes what ⌘Z undid; a redone operation can be undone again; a new operation
/// (on a worker or on this thread) ends what could be redone.
#[test]
fn redo_after_undo_and_a_new_operation_clears_it() {
    let d = folder("redo", 0);
    std::fs::write(d.join("a.txt"), "x").unwrap();
    let mut s = state_in(&d);
    let mut tx = Tx::default();
    assert_eq!(s.unavailable(Cmd::Redo), Some("Nothing to redo"));
    s.run(Cmd::Duplicate, &mut tx);
    finish_job(&mut s);
    assert!(d.join("a copy.txt").exists());
    s.run(Cmd::Undo, &mut tx);
    finish_job(&mut s);
    assert!(!d.join("a copy.txt").exists());
    assert_eq!((s.shared.undo.borrow().len(), s.shared.redo.borrow().len()), (0, 1));
    assert_eq!(s.unavailable(Cmd::Redo), None);
    s.run(Cmd::Redo, &mut tx);
    finish_job(&mut s);
    assert!(d.join("a copy.txt").exists(), "redone");
    assert_eq!(s.note.as_deref(), Some("Made “a copy.txt” again"));
    assert_eq!((s.shared.undo.borrow().len(), s.shared.redo.borrow().len()), (1, 0));
    // Undo it again, then a new operation: nothing to redo any more.
    s.run(Cmd::Undo, &mut tx);
    finish_job(&mut s);
    assert_eq!(s.shared.redo.borrow().len(), 1);
    s.new_folder();
    assert!(s.shared.redo.borrow().is_empty(), "a new folder ends the redo");
    finish_job(&mut s);
    s.rename = None;
    s.run(Cmd::Undo, &mut tx);
    finish_job(&mut s);
    assert_eq!(s.shared.redo.borrow().len(), 1);
    let i = s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].name == "a.txt").unwrap();
    s.cur_mut().select(i);
    s.run(Cmd::Duplicate, &mut tx);
    assert!(s.shared.redo.borrow().is_empty(), "so does a copy");
    finish_job(&mut s);
}

#[test]
fn file_operations_are_unavailable_while_one_runs() {
    let d = folder("busy", 2);
    let mut s = state_in(&d);
    assert_eq!(s.unavailable(Cmd::Duplicate), None);
    assert_eq!(s.unavailable(Cmd::Cancel), Some("No file operation is running"));
    s.set_job(crate::ops::start(crate::ops::Op::Copy { sources: Vec::new(), dest: d.clone() }, None, None));
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

/// A go-to looks at what's at its path on a worker: a path on a disk that doesn't answer
/// holds up nothing (it used to be looked at here, on the UI thread), and it goes there when
/// the answer lands; a newer go-to wins over it.
#[test]
fn a_typed_path_is_looked_at_off_the_ui_thread() {
    let (a, b) = (folder("goto-a", 1), folder("goto-b", 1));
    let mut s = state_in(&a);
    let (go, gate) = std::sync::mpsc::channel();
    path_hook::GATES.lock().unwrap().push((b.clone(), gate));
    let t = Instant::now();
    s.go_path(&b);
    assert!(t.elapsed() < Duration::from_millis(500), "waited {:?}", t.elapsed());
    assert!(s.going_to.is_some() && !s.is_loading() && s.cur().cwd == a, "nothing happens until it's known");
    go.send(()).unwrap();
    settle_all(&mut s);
    assert_eq!(s.cur().cwd, b);
    // Two go-tos: only the newer acts, even when the older answers last.
    let (go, gate) = std::sync::mpsc::channel();
    path_hook::GATES.lock().unwrap().push((a.clone(), gate));
    s.go_path(&a);
    s.go_path(&a.join("missing"));
    go.send(()).unwrap();
    settle_all(&mut s);
    assert_eq!(s.cur().cwd, b, "the older go-to didn't act");
    assert!(s.note.as_deref().is_some_and(|n| n.starts_with("No such folder")), "{:?}", s.note);
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
    let file = Settings { show_hidden: false, sort: Sort::new(SortKey::Kind, false), ..Settings::default() };
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
    let file = Settings { show_hidden: true, sort: Sort::new(SortKey::Kind, false), ..Settings::default() };
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
    let late = |req: u64, p: &Path| Answer::new(req, Settings::default().sort, fs::list(p.to_path_buf(), Settings::default().sort));
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
    let a = Answer::new(req, by_name, fs::list(d.clone(), by_name));
    assert!(s.land(0, a, &mut Tx::default()));
    let names: Vec<&str> = s.cur().entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["c", "b", "a"]);
}

/// The review of milestone 14: renaming `a` to a name already taken pointed the row (and Open,
/// Copy, everything acting on it) at the other item until the refusal landed. Now the new name
/// is only a label until the worker says how it went: the row is still `a`, actions on it wait,
/// and a refused rename shows `a` again.
#[test]
fn a_pending_rename_never_points_the_row_at_another_item() {
    let d = folder("rename-pending", 0);
    std::fs::write(d.join("a"), "original").unwrap();
    std::fs::write(d.join("b"), "other").unwrap();
    let mut s = state_in(&d);
    settle_all(&mut s);
    s.cur_mut().select(0);
    assert_eq!(s.cur().target_paths(), [d.join("a")]);
    s.start_rename();
    s.rename.as_mut().unwrap().edit.text = "b".into();
    s.commit_rename(&mut Tx::default());
    assert!(s.owns_job());
    assert_eq!(s.cur().target_paths(), [d.join("a")], "still the item being renamed");
    assert_eq!(s.cur().selected().and_then(|e| e.pending.as_deref()), Some("b"), "its new name, as a label");
    for cmd in [Cmd::Open, Cmd::Copy, Cmd::Cut, Cmd::Rename, Cmd::OpenWith, Cmd::CopyPath, Cmd::Trash, Cmd::Delete] {
        assert!(s.unavailable(cmd).is_some(), "{cmd:?} waits for the rename");
    }
    finish_job(&mut s);
    assert_eq!(s.note.as_deref(), Some("“b” already exists"));
    assert_eq!(s.cur().target_paths(), [d.join("a")]);
    assert!(s.cur().entries.iter().all(|e| e.pending.is_none()), "the label went");
    assert_eq!(s.unavailable(Cmd::Open), None);
    assert_eq!(std::fs::read_to_string(d.join("b")).unwrap(), "other");
}

/// The review's case: the inline rename and New Folder touched the disk on the window's
/// thread (a slow disk froze it). Both are operations on the worker now: the list shows the
/// new name at once, the disk changes when the worker gets to it, undo is as before, and
/// while another operation runs the rename field stays open (and says why).
#[test]
fn rename_and_new_folder_run_on_the_worker() {
    let d = folder("rename-worker", 0);
    std::fs::write(d.join("a.txt"), "x").unwrap();
    let mut s = state_in(&d);
    settle_all(&mut s);
    let mut tx = Tx::default();
    s.start_rename();
    s.rename.as_mut().unwrap().edit.text = "b.txt".into();
    s.commit_rename(&mut tx);
    assert!(s.owns_job(), "on the worker");
    assert_eq!(s.cur().selected().and_then(|e| e.pending.as_deref()), Some("b.txt"), "shown at once");
    finish_job(&mut s);
    assert!(d.join("b.txt").exists() && !d.join("a.txt").exists());
    assert_eq!((s.note.as_deref(), s.shared.undo.borrow().len()), (None, 1), "nothing to say; undoable");
    s.new_folder();
    assert!(s.owns_job(), "on the worker");
    finish_job(&mut s);
    assert!(d.join("untitled folder").is_dir());
    assert!(s.rename.is_some(), "renaming the new folder once it's listed");
    assert_eq!(s.shared.undo.borrow().len(), 2);
    s.rename = None;
    // A refused rename says why, and the list shows what's on disk again.
    std::fs::write(d.join("c.txt"), "c").unwrap();
    s.reload();
    settle_all(&mut s);
    let i = s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].name == "b.txt").unwrap();
    s.cur_mut().select(i);
    s.start_rename();
    s.rename.as_mut().unwrap().edit.text = "c.txt".into();
    s.commit_rename(&mut tx);
    finish_job(&mut s);
    assert_eq!(s.note.as_deref(), Some("“c.txt” already exists"));
    assert!(s.cur().entries.iter().any(|e| e.name == "b.txt"), "the list put right");
    assert_eq!(std::fs::read_to_string(d.join("c.txt")).unwrap(), "c", "never over it");
    // Busy: the field stays open.
    s.set_job(crate::ops::start(crate::ops::Op::Copy { sources: Vec::new(), dest: d.clone() }, None, None));
    s.start_rename();
    s.commit_rename(&mut tx);
    assert!(s.rename.is_some() && s.note.as_deref() == Some(BUSY));
    assert_eq!(s.unavailable(Cmd::NewFolder), Some(BUSY));
}

/// A file edited after its folder was listed (Arbor doesn't watch folders): Delete Permanently,
/// which checks each item against its identity from the listing, still deletes it, and Trash
/// and the inline rename still go ahead.
#[test]
fn an_item_edited_after_the_listing_is_still_acted_on() {
    let d = folder("edited-after-listing", 0);
    for n in ["a-delete", "b-trash", "c-rename"] {
        std::fs::write(d.join(n), "as listed").unwrap();
    }
    let mut s = state_in(&d);
    settle_all(&mut s);
    for n in ["a-delete", "b-trash", "c-rename"] {
        std::fs::write(d.join(n), "edited after the listing, and longer").unwrap();
        let t = std::time::SystemTime::now() + Duration::from_secs(3600);
        std::fs::File::options().write(true).open(d.join(n)).unwrap().set_modified(t).unwrap();
    }
    let at = |s: &State, n: &str| s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].name == n).unwrap();
    let ok = |s: &State, what: &str| assert!(!s.note.as_deref().is_some_and(|n| n.contains("left alone")), "{what}: {:?}", s.note);
    let i = at(&s, "a-delete");
    s.cur_mut().select(i);
    s.run(Cmd::Delete, &mut Tx::default());
    s.answer(true, &mut Tx::default());
    finish_job(&mut s);
    ok(&s, "delete");
    assert!(!d.join("a-delete").exists(), "delete");
    settle_all(&mut s);
    let i = at(&s, "b-trash");
    s.cur_mut().select(i);
    s.run(Cmd::Trash, &mut Tx::default());
    finish_job(&mut s);
    ok(&s, "trash");
    assert!(!d.join("b-trash").exists(), "trash");
    settle_all(&mut s);
    let i = at(&s, "c-rename");
    s.cur_mut().select(i);
    s.start_rename();
    s.rename.as_mut().unwrap().edit.text = "c-renamed".into();
    s.commit_rename(&mut Tx::default());
    finish_job(&mut s);
    ok(&s, "rename");
    assert!(d.join("c-renamed").exists() && !d.join("c-rename").exists(), "rename");
    let _ = std::fs::remove_dir_all(&d);
}

/// The review's case: a row of a folder's listing whose file was replaced since (another app
/// saving over it by rename, or deleting it and making another) was trashed, renamed, moved or
/// copied as the newcomer: only search results were checked. Now every action checks each item
/// against the identity its listing read, and says to read the folder again.
#[test]
fn a_listed_item_replaced_since_is_left_alone() {
    let d = folder("replaced-since-listed", 0);
    let into = folder("replaced-since-listed-into", 0);
    let names = ["a-trash", "b-rename", "c-cut", "d-f6", "e-f5", "f-delete", "g-drag"];
    for n in names {
        std::fs::write(d.join(n), "as listed").unwrap();
    }
    let mut s = state_in(&d);
    settle_all(&mut s);
    s.run(Cmd::Split, &mut Tx::default());
    settle_all(&mut s);
    s.panes[1].f.cwd = into.clone();
    s.panes[1].f.dir_id = crate::noclobber::dir_id(&into);
    s.activate(0, &mut Tx::default());
    let at = |s: &State, n: &str| s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].name == n).unwrap();
    // The folder as listed, then the item replaced: what each action finds at its path.
    let replaced = |s: &mut State, n: &str| {
        settle_all(s);
        let i = at(s, n);
        s.cur_mut().select(i);
        crate::search::tests::replace(&d.join(n));
        s.note = None;
    };
    let untouched = |s: &State, n: &str, said: &str| {
        assert_eq!(std::fs::read_to_string(d.join(n)).unwrap(), "SOMEONE ELSE'S FILE", "{n}: the newcomer stays");
        assert!(s.note.as_deref().is_some_and(|t| t.contains(said)), "{n}: {:?}", s.note);
    };
    let since = "was replaced since the folder was read";
    replaced(&mut s, "a-trash");
    s.run(Cmd::Trash, &mut Tx::default());
    finish_job(&mut s);
    untouched(&s, "a-trash", since);
    replaced(&mut s, "b-rename");
    s.start_rename();
    s.rename.as_mut().unwrap().edit.text = "b-renamed".into();
    s.commit_rename(&mut Tx::default());
    finish_job(&mut s);
    untouched(&s, "b-rename", since);
    assert!(!d.join("b-renamed").exists());
    replaced(&mut s, "c-cut");
    s.run(Cmd::Cut, &mut Tx::default());
    let content = rune::ClipboardContent::new("").with_files(s.shared.clip.borrow().as_ref().unwrap().paths.clone());
    s.paste_reqs.push(PasteReq { id: 0, dest: into.clone(), dest_id: None, mv: false });
    s.paste_reply(0, &content);
    finish_job(&mut s);
    untouched(&s, "c-cut", since);
    replaced(&mut s, "d-f6");
    s.run(Cmd::MoveToOther, &mut Tx::default());
    finish_job(&mut s);
    untouched(&s, "d-f6", since);
    replaced(&mut s, "e-f5");
    s.run(Cmd::CopyToOther, &mut Tx::default());
    finish_job(&mut s);
    untouched(&s, "e-f5", since);
    assert!(std::fs::read_dir(&into).unwrap().next().is_none(), "nothing went into the other folder");
    replaced(&mut s, "f-delete");
    s.run(Cmd::Delete, &mut Tx::default());
    s.answer(true, &mut Tx::default());
    finish_job(&mut s);
    untouched(&s, "f-delete", "isn't the item you chose");
    // A drag starts at once with the row as listed; dropped on the other pane, the worker
    // leaves the newcomer alone.
    replaced(&mut s, "g-drag");
    let (i, id, version, to) = (s.cur().sel, s.cur().id, s.cur().version, s.panes[1].id());
    assert!(s.drag_item(id, i).is_some(), "a drag starts");
    s.drop_on_pane(id, i, version, to, &mut Tx::default());
    finish_job(&mut s);
    untouched(&s, "g-drag", since);
    assert!(std::fs::read_dir(&into).unwrap().next().is_none(), "nothing went into the other folder");
    // Read again, the newcomer is what the row is: it goes.
    settle_all(&mut s);
    s.run(Cmd::Reload, &mut Tx::default());
    settle_all(&mut s);
    let i = at(&s, "a-trash");
    s.cur_mut().select(i);
    s.run(Cmd::Trash, &mut Tx::default());
    finish_job(&mut s);
    assert!(!d.join("a-trash").exists(), "{:?}", s.note);
    let _ = std::fs::remove_dir_all(&d);
    let _ = std::fs::remove_dir_all(&into);
}

/// The review's case: starting a drag stat'ed (on Windows, opened) every selected item on the
/// window's thread, so a slow disk froze the window. Now it carries the rows as listed and
/// reads nothing: here the folder is gone and the drag still starts, with every path.
#[test]
fn starting_a_drag_reads_nothing_from_the_disk() {
    let d = folder("drag-no-io", 3);
    let mut s = state_in(&d);
    settle_all(&mut s);
    s.run(Cmd::SelectAll, &mut Tx::default());
    std::fs::remove_dir_all(&d).unwrap();
    let before = crate::fs::reads::so_far();
    let (i, id) = (s.cur().sel, s.cur().id);
    let item = s.drag_item(id, i).expect("the drag starts");
    assert_eq!(crate::fs::reads::so_far(), before, "no identity read");
    assert_eq!(item.paths().len(), 3, "every listed path, unread");
}

/// The review's case: a row whose identity couldn't be read when its folder was listed (here,
/// a folder readable but not searchable then) had no check at all, so trash, rename, a move and
/// a drop went ahead on whatever was at its path later. Now they refuse until the folder is
/// read again; a copy, which leaves it where it is, goes ahead.
#[cfg(unix)]
#[test]
fn a_row_listed_without_an_identity_is_never_moved_or_removed() {
    use std::os::unix::fs::PermissionsExt;
    let d = folder("unverified", 0);
    let into = folder("unverified-into", 0);
    let names = ["a-trash", "b-rename", "c-cut", "d-drop", "e-batch", "f-copy"];
    for n in names {
        std::fs::write(d.join(n), "as listed").unwrap();
    }
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o400)).unwrap();
    let mut s = state_in(&d);
    settle_all(&mut s);
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
    if s.cur().entries.iter().any(|e| e.id.is_some()) {
        eprintln!("a_row_listed_without_an_identity_is_never_moved_or_removed: skipped (permissions don't stop lstat here: root?)");
        return;
    }
    s.run(Cmd::Split, &mut Tx::default());
    settle_all(&mut s);
    s.panes[1].f.cwd = into.clone();
    s.panes[1].f.dir_id = crate::noclobber::dir_id(&into);
    s.activate(0, &mut Tx::default());
    for n in names {
        crate::search::tests::replace(&d.join(n));
    }
    let at = |s: &State, n: &str| s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].name == n).unwrap();
    let pick = |s: &mut State, n: &str| {
        let i = at(s, n);
        s.cur_mut().select(i);
        s.note = None;
    };
    let untouched = |s: &State, n: &str| {
        assert_eq!(std::fs::read_to_string(d.join(n)).unwrap(), "SOMEONE ELSE'S FILE", "{n}: the newcomer stays");
        assert!(s.note.as_deref().is_some_and(|t| t.contains("Couldn't verify")), "{n}: {:?}", s.note);
    };
    pick(&mut s, "a-trash");
    s.run(Cmd::Trash, &mut Tx::default());
    finish_job(&mut s);
    untouched(&s, "a-trash");
    pick(&mut s, "b-rename");
    s.start_rename();
    s.rename.as_mut().unwrap().edit.text = "b-renamed".into();
    s.commit_rename(&mut Tx::default());
    finish_job(&mut s);
    untouched(&s, "b-rename");
    pick(&mut s, "c-cut");
    s.run(Cmd::Cut, &mut Tx::default());
    let content = rune::ClipboardContent::new("").with_files(s.shared.clip.borrow().as_ref().unwrap().paths.clone());
    s.paste_reqs.push(PasteReq { id: 0, dest: into.clone(), dest_id: None, mv: false });
    s.paste_reply(0, &content);
    finish_job(&mut s);
    untouched(&s, "c-cut");
    pick(&mut s, "d-drop");
    let (i, id, version, to) = (s.cur().sel, s.cur().id, s.cur().version, s.panes[1].id());
    s.drop_on_pane(id, i, version, to, &mut Tx::default());
    finish_job(&mut s);
    untouched(&s, "d-drop");
    assert!(std::fs::read_dir(&into).unwrap().next().is_none(), "nothing went into the other folder");
    pick(&mut s, "e-batch");
    s.run(Cmd::BatchRename, &mut Tx::default());
    crate::batch::tests::settle_batch(&mut s);
    let b = s.batch.as_ref().unwrap();
    assert!(matches!(&b.plan.status[0], crate::rename::Status::Problem(w) if w.contains("couldn't be verified")), "{:?}", b.plan.status);
    s.close_batch(&mut Tx::default());
    pick(&mut s, "f-copy");
    s.run(Cmd::CopyToOther, &mut Tx::default());
    finish_job(&mut s);
    assert_eq!(std::fs::read_to_string(into.join("f-copy")).unwrap(), "SOMEONE ELSE'S FILE", "a copy goes ahead: {:?}", s.note);
    let _ = std::fs::remove_dir_all(&d);
    let _ = std::fs::remove_dir_all(&into);
}

/// What an operation's own change made of an item's identity (`Outcome::reidentified`) reaches
/// every row still showing the old one, under any name (the item renamed, other hard links to
/// it); other rows keep theirs.
#[test]
fn rows_take_the_identities_arbors_change_gave_them() {
    let d = folder("reidentify", 3);
    let mut s = state_in(&d);
    settle_all(&mut s);
    let id = |s: &State, n: &str| s.cur().entries.iter().find(|e| e.name == n).unwrap().id.unwrap();
    let (a, b, c) = (id(&s, "f0"), id(&s, "f1"), id(&s, "f2"));
    // (Any identity will do as "what Arbor made of it".)
    s.cur_mut().reidentify(&[(a, c)]);
    assert_eq!((id(&s, "f0"), id(&s, "f1"), id(&s, "f2")), (c, b, c));
    let _ = std::fs::remove_dir_all(&d);
}

/// Go To (⌘L), cleared with Backspace held down: once the field is empty, more Backspaces
/// (and Enter) do nothing, and the field stays open saying so, rather than going to the parent
/// folder as Backspace does with no field (real-Mac QA: it landed on `/`). Esc closes the
/// field; then Backspace goes up again.
#[test]
fn backspace_in_an_empty_go_to_field_stays_in_the_field() {
    let d = folder("goto-backspace", 2);
    let mut s = state_in(&d);
    settle(&mut s);
    let mut tx = Tx::default();
    let key = |k: Key| KeyEvent { key: k, mods: rune::Mods::default() };
    s.run(Cmd::GoTo, &mut tx);
    let typed = s.cur().filter.chars().count();
    assert!(typed > 0 && s.cur().path_mode());
    for _ in 0..typed + 5 {
        assert!(s.on_key(&key(Key::Backspace), &mut tx), "the field takes it");
    }
    assert!(s.on_key(&key(Key::Enter), &mut tx), "Enter in the empty field does nothing");
    assert_eq!((s.cur().cwd.as_path(), s.pending_path()), (d.as_path(), None), "no navigation");
    assert!(s.cur().filter.is_empty() && s.cur().goto, "the field is still open");
    // Typing in it carries on; a letter is a filter, as ever.
    s.on_key(&key(Key::Text("f".into())), &mut tx);
    assert_eq!(s.cur().filter, "f");
    assert!(!s.cur().goto && !s.cur().path_mode());
    // Esc closes an empty go-to field; after that Backspace goes to the parent folder.
    s.run(Cmd::GoTo, &mut tx);
    for _ in 0..typed + 1 {
        s.on_key(&key(Key::Backspace), &mut tx);
    }
    assert!(s.on_key(&key(Key::Escape), &mut tx));
    assert!(!s.cur().goto);
    s.on_key(&key(Key::Backspace), &mut tx);
    assert_eq!(s.pending_path(), d.parent(), "Backspace with no field: the parent folder");
    let _ = std::fs::remove_dir_all(&d);
}

/// Backspaces past the end of an open go-to field (`typed` characters of it): none of them
/// goes anywhere, and the field stays open.
fn backspace_out(s: &mut State, typed: usize, d: &Path) {
    let mut tx = Tx::default();
    for _ in 0..typed + 5 {
        assert!(s.on_key(&KeyEvent { key: Key::Backspace, mods: rune::Mods::default() }, &mut tx));
    }
    assert_eq!((s.cur().cwd.as_path(), s.pending_path()), (d, None), "no navigation");
    assert!(s.cur().filter.is_empty() && s.cur().goto, "the field is still open");
}

/// The go-to field keeps its mode with its text: through a reload (⌘R), and through its tab
/// being hidden and shown again. Emptied by Backspace afterwards, it still stays open.
#[test]
fn an_open_go_to_field_survives_a_reload_and_a_tab_switch() {
    let d = folder("goto-keep", 2);
    let mut s = state_in(&d);
    settle(&mut s);
    let mut tx = Tx::default();
    s.run(Cmd::GoTo, &mut tx);
    let typed = s.cur().filter.chars().count();
    s.run(Cmd::Reload, &mut tx);
    settle(&mut s);
    assert!(s.cur().goto && s.cur().filter.chars().count() == typed, "kept through the reload");
    backspace_out(&mut s, typed, &d);
    // Hidden and shown again.
    s.run(Cmd::Clear, &mut tx);
    s.run(Cmd::GoTo, &mut tx);
    s.run(Cmd::NewTab, &mut tx);
    s.run(Cmd::PrevTab, &mut tx);
    settle(&mut s);
    assert!(s.cur().goto && s.cur().filter.chars().count() == typed, "kept in the tab's memo");
    backspace_out(&mut s, typed, &d);
    let _ = std::fs::remove_dir_all(&d);
}

/// A new tab starts with no go-to field open, whatever the tab it came from had: ⌘L then ⌘T,
/// and Backspace in the new tab goes to the parent folder, as with no field.
#[test]
fn a_new_tab_starts_with_no_go_to_field() {
    let d = folder("goto-new-tab", 2);
    let mut s = state_in(&d);
    settle(&mut s);
    let mut tx = Tx::default();
    s.run(Cmd::GoTo, &mut tx);
    s.run(Cmd::NewTab, &mut tx);
    assert!(s.cur().filter.is_empty() && !s.cur().goto, "no field in the new tab");
    s.on_key(&KeyEvent { key: Key::Backspace, mods: rune::Mods::default() }, &mut tx);
    assert_eq!(s.pending_path(), d.parent(), "Backspace goes to the parent folder");
    let _ = std::fs::remove_dir_all(&d);
}

// ---- New File, Make Link, Show Original ------------------------------------------------------

/// New File makes "untitled.txt" on the worker and, once it's listed, renames it inline with
/// the name before its extension selected, in each view: list, icons, tiles, and in the
/// column view in the active column's folder.
#[test]
fn new_file_starts_its_rename_in_every_view() {
    for (i, view) in [Cmd::AsList, Cmd::AsIcons, Cmd::AsTiles, Cmd::AsColumns].into_iter().enumerate() {
        let d = folder(&format!("new-file-view-{i}"), 2);
        std::fs::create_dir(d.join("Docs")).unwrap();
        std::fs::write(d.join("Docs/readme"), "x").unwrap();
        let mut s = state_in(&d);
        settle_all(&mut s);
        let mut tx = Tx::default();
        if view != Cmd::AsList {
            s.run(view, &mut tx);
            settle_all(&mut s);
        }
        let here = if view == Cmd::AsColumns {
            // Into Docs: the active column.
            let i = s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].name == "Docs").unwrap();
            s.cur_mut().select(i);
            s.run(Cmd::Into, &mut tx);
            settle_all(&mut s);
            assert!(s.cur().cols.is_some() && s.cur().cwd == d.join("Docs"), "{view:?}");
            d.join("Docs")
        } else {
            d.clone()
        };
        assert_eq!(s.unavailable(Cmd::NewFile), None);
        s.run(Cmd::NewFile, &mut tx);
        assert!(s.owns_job(), "made on the worker ({view:?})");
        finish_job(&mut s);
        assert_eq!(std::fs::read(here.join("untitled.txt")).unwrap(), b"", "{view:?}");
        let r = s.rename.as_ref().unwrap_or_else(|| panic!("renaming the new file in {view:?}"));
        assert_eq!(r.os, "untitled.txt");
        assert_eq!((r.edit.anchor.min(r.edit.focus), r.edit.anchor.max(r.edit.focus)), (0, "untitled".len()), "the stem is selected");
        assert_eq!(s.note, None, "nothing to say ({view:?})");
        // Typing a name renames it; undo of that, then of the new file, leaves nothing.
        s.rename.as_mut().unwrap().edit.text = "notes.md".into();
        s.commit_rename(&mut tx);
        finish_job(&mut s);
        assert!(here.join("notes.md").exists() && !here.join("untitled.txt").exists(), "{view:?}");
        assert_eq!(s.shared.undo.borrow().len(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// The folder on screen was replaced since it was listed: New File (and New Folder) make
/// nothing, in either folder, and say why.
#[test]
fn new_file_refuses_a_folder_replaced_since_it_was_listed() {
    let root = folder("new-file-replaced", 0);
    let d = root.join("here");
    std::fs::create_dir(&d).unwrap();
    let mut s = state_in(&d);
    settle_all(&mut s);
    std::fs::rename(&d, root.join("moved")).unwrap();
    std::fs::create_dir(&d).unwrap();
    for cmd in [Cmd::NewFile, Cmd::NewFolder] {
        s.run(cmd, &mut Tx::default());
        finish_job(&mut s);
        let note = s.note.clone().unwrap_or_default();
        assert!(note.starts_with("Nothing was made: “here” is another folder now"), "{cmd:?}: {note}");
        assert!(s.rename.is_none());
    }
    assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(root.join("moved")).unwrap().count(), 0);
    let _ = std::fs::remove_dir_all(&root);
}

/// Make Symbolic Link: a link beside each selected item, selected once listed; one undo
/// removes them; Show Original on a link selects its item, in another folder too.
#[cfg(unix)]
#[test]
fn make_link_selects_the_links_and_show_original_finds_the_item() {
    let d = folder("make-link", 0);
    std::fs::write(d.join("a.txt"), "a").unwrap();
    std::fs::write(d.join("b.txt"), "b").unwrap();
    std::fs::create_dir(d.join("sub")).unwrap();
    std::fs::write(d.join("sub/deep.txt"), "d").unwrap();
    std::os::unix::fs::symlink("../a.txt", d.join("sub/rel")).unwrap();
    std::os::unix::fs::symlink(d.join("gone"), d.join("broken")).unwrap();
    let mut s = state_in(&d);
    settle_all(&mut s);
    let mut tx = Tx::default();
    assert_eq!(s.unavailable(Cmd::MakeLink), None, "the cursor's item");
    let pick = |s: &mut State, name: &str| {
        let i = s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].name == name).unwrap();
        s.cur_mut().select(i);
    };
    pick(&mut s, "a.txt");
    s.cur_mut().mark_names(&["a.txt", "b.txt"]);
    assert_eq!(s.cur().target_paths().len(), 2);
    assert!(s.menu_items(true).iter().flatten().any(|(_, t, on)| t == "Make Symbolic Link (2 items)" && *on));
    s.run(Cmd::MakeLink, &mut tx);
    finish_job(&mut s);
    assert_eq!(s.note.as_deref(), Some("Made 2 links"));
    assert_eq!(std::fs::read_link(d.join("a link.txt")).unwrap(), d.join("a.txt"));
    let mut picked: Vec<PathBuf> = s.cur().target_paths();
    picked.sort();
    assert_eq!(picked, [d.join("a link.txt"), d.join("b link.txt")], "the links are selected");
    // Show Original: only on a link.
    pick(&mut s, "a.txt");
    assert!(s.unavailable(Cmd::ShowOriginal).is_some());
    assert!(!s.menu_items(true).iter().flatten().any(|(a, ..)| *a == crate::menu::Act::Cmd(Cmd::ShowOriginal)));
    pick(&mut s, "b link.txt");
    assert!(s.menu_items(true).iter().flatten().any(|(a, ..)| *a == crate::menu::Act::Cmd(Cmd::ShowOriginal)));
    s.run(Cmd::ShowOriginal, &mut tx);
    settle_all(&mut s);
    assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some("b.txt"), "selected in place");
    // A relative link in another folder: its item, in its own folder (⌘[ comes back).
    s.go(d.join("sub"), Nav::New, None);
    settle_all(&mut s);
    pick(&mut s, "rel");
    s.run(Cmd::ShowOriginal, &mut tx);
    settle_all(&mut s);
    assert_eq!(s.cur().cwd, std::fs::canonicalize(&d).unwrap());
    assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some("a.txt"));
    assert_eq!(s.cur().back.last(), Some(&d.join("sub")));
    // A link to nothing says so.
    pick(&mut s, "broken");
    s.run(Cmd::ShowOriginal, &mut tx);
    settle_all(&mut s);
    assert_eq!(s.note.as_deref(), Some(format!("The original isn't there: {}", fs::pretty(&d.join("gone"))).as_str()));
    // Undo the links.
    s.run(Cmd::Undo, &mut tx);
    finish_job(&mut s);
    assert!(d.join("a link.txt").symlink_metadata().is_err() && d.join("b link.txt").symlink_metadata().is_err());
    let _ = std::fs::remove_dir_all(&d);
}

/// Where a link points, as Show Original reads it: `..` resolved, a missing target broken.
#[cfg(unix)]
#[test]
fn originals_resolve_relative_targets() {
    let d = folder("originals", 0);
    std::fs::create_dir_all(d.join("x/y")).unwrap();
    std::fs::write(d.join("x/t"), "t").unwrap();
    std::os::unix::fs::symlink("../t", d.join("x/y/up")).unwrap();
    std::os::unix::fs::symlink("y/../t", d.join("x/mid")).unwrap();
    std::os::unix::fs::symlink("nowhere", d.join("x/none")).unwrap();
    std::os::unix::fs::symlink("t", d.join("x/plain")).unwrap();
    std::os::unix::fs::symlink(d.join("x/t"), d.join("abs")).unwrap();
    let canon = std::fs::canonicalize(&d).unwrap();
    assert_eq!(original(&d.join("x/y/up")), (canon.join("x/t"), PathKind::Original), "`..` through the real folders");
    assert_eq!(original(&d.join("x/mid")), (d.join("x/t"), PathKind::Original), "back in the link's folder: its spelling");
    assert_eq!(original(&d.join("x/plain")), (d.join("x/t"), PathKind::Original), "as written");
    assert_eq!(original(&d.join("abs")), (d.join("x/t"), PathKind::Original), "as written");
    assert_eq!(original(&d.join("x/none")), (d.join("x/nowhere"), PathKind::Broken));
    assert_eq!(original(&d.join("x/t")).1, PathKind::Broken, "not a link");
    assert_eq!(lexical(Path::new("/a/b/../c/./d")), Path::new("/a/c/d"));
    let _ = std::fs::remove_dir_all(&d);
}

/// The review's case: with a filter that hides "untitled.txt" ("rep" shows report.txt), New
/// File used to open the rename on report.txt. The filter is cleared so the new file shows,
/// and the rename starts on it, in every view.
#[test]
fn new_file_under_a_filter_renames_the_new_file_not_another() {
    for (i, view) in [Cmd::AsList, Cmd::AsIcons, Cmd::AsTiles, Cmd::AsColumns].into_iter().enumerate() {
        let d = folder(&format!("new-file-filter-{i}"), 0);
        std::fs::write(d.join("report.txt"), "r").unwrap();
        std::fs::write(d.join("notes.md"), "n").unwrap();
        let mut s = state_in(&d);
        settle_all(&mut s);
        let mut tx = Tx::default();
        if view != Cmd::AsList {
            s.run(view, &mut tx);
            settle_all(&mut s);
        }
        s.cur_mut().type_filter("rep");
        assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some("report.txt"));
        s.run(Cmd::NewFile, &mut tx);
        finish_job(&mut s);
        assert!(d.join("untitled.txt").is_file());
        assert_eq!(s.cur().filter, "", "the filter that hid it is cleared ({view:?})");
        assert_eq!(s.rename.as_ref().map(|r| r.os.clone()), Some("untitled.txt".into()), "{view:?}");
        assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some("untitled.txt"));
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// The new file moved away before the reload lands: no rename starts at all (it used to open
/// on whatever row was selected instead).
#[test]
fn new_file_starts_no_rename_on_a_row_that_isnt_it() {
    let d = folder("new-file-gone", 0);
    std::fs::write(d.join("a.txt"), "a").unwrap();
    let mut s = state_in(&d);
    settle_all(&mut s);
    // The disk changes after the file is made and before the operation lands (which asks for
    // the reload), so the reload reads the change.
    let made_then = |s: &mut State, change: &dyn Fn()| {
        s.run(Cmd::NewFile, &mut Tx::default());
        let until = Instant::now() + Duration::from_secs(10);
        while !s.shared.job.borrow().as_ref().is_some_and(|j| j.run.finished()) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
        change();
        wait_job(s);
        assert!(s.is_loading(), "the reload is on its way");
        settle_all(s);
    };
    made_then(&mut s, &|| std::fs::rename(d.join("untitled.txt"), d.join("moved.txt")).unwrap());
    assert!(s.rename.is_none(), "renaming {:?}", s.rename.as_ref().map(|r| r.os.clone()));
    // Another file at its name meanwhile isn't it either (made past the clock tick the new
    // file was made in, so its birth time is its own).
    made_then(&mut s, &|| {
        std::fs::remove_file(d.join("untitled.txt")).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(d.join("untitled.txt"), "someone else's").unwrap();
    });
    assert!(s.rename.is_none(), "renaming {:?}", s.rename.as_ref().map(|r| r.os.clone()));
    let _ = std::fs::remove_dir_all(&d);
}

/// The review's case: Show Original under a filter that hides the original ("rl" shows the
/// link alone) left the link selected. The filter is cleared and the original selected; a
/// hidden original turns hidden files on (and says so), in this folder or another.
#[cfg(unix)]
#[test]
fn show_original_clears_a_filter_and_shows_a_hidden_original() {
    let d = folder("original-filter", 0);
    std::fs::write(d.join("report.txt"), "r").unwrap();
    std::fs::write(d.join(".secret"), "s").unwrap();
    std::fs::create_dir(d.join("sub")).unwrap();
    std::fs::write(d.join("sub/.deep"), "d").unwrap();
    std::os::unix::fs::symlink(d.join("report.txt"), d.join("rlink")).unwrap();
    std::os::unix::fs::symlink(d.join(".secret"), d.join("slink")).unwrap();
    std::os::unix::fs::symlink(d.join("sub/.deep"), d.join("dlink")).unwrap();
    let mut s = state_in(&d);
    settle_all(&mut s);
    let mut tx = Tx::default();
    assert!(!s.show_hidden);
    s.cur_mut().type_filter("rl");
    assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some("rlink"));
    s.run(Cmd::ShowOriginal, &mut tx);
    settle_all(&mut s);
    assert_eq!(s.cur().filter, "", "the filter that hid it is cleared");
    assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some("report.txt"));
    // Hidden, here.
    let pick = |s: &mut State, name: &str| {
        let i = s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].name == name).unwrap();
        s.cur_mut().select(i);
    };
    pick(&mut s, "slink");
    s.run(Cmd::ShowOriginal, &mut tx);
    settle_all(&mut s);
    assert!(s.show_hidden, "hidden files are shown to show it");
    assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some(".secret"));
    assert!(s.note.as_deref().is_some_and(|n| n.contains("hidden")), "{:?}", s.note);
    // Hidden, in another folder.
    s.toggle_hidden();
    settle_all(&mut s);
    pick(&mut s, "dlink");
    s.run(Cmd::ShowOriginal, &mut tx);
    settle_all(&mut s);
    assert_eq!(s.cur().cwd, d.join("sub"));
    assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some(".deep"));
    let _ = std::fs::remove_dir_all(&d);
}

/// Undo and redo say what they did (the real-Mac QA: undoing New File or Make Link said
/// "Restored 1 item").
#[cfg(unix)]
#[test]
fn undo_and_redo_say_what_they_did() {
    let d = folder("undo-words", 0);
    std::fs::write(d.join("a.txt"), "a").unwrap();
    let mut s = state_in(&d);
    settle_all(&mut s);
    let mut tx = Tx::default();
    let mut step = |s: &mut State, cmd: Cmd| {
        s.rename = None;
        s.run(cmd, &mut tx);
        finish_job(s);
        s.note.clone().unwrap_or_default()
    };
    step(&mut s, Cmd::NewFile);
    let bin = crate::trash::BIN;
    assert_eq!(step(&mut s, Cmd::Undo), format!("Moved “untitled.txt” to the {bin}"));
    assert_eq!(step(&mut s, Cmd::Redo), "Made “untitled.txt” again");
    step(&mut s, Cmd::Undo);
    let i = s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].name == "a.txt").unwrap();
    s.cur_mut().select(i);
    assert_eq!(step(&mut s, Cmd::MakeLink), "Made 1 link");
    assert_eq!(step(&mut s, Cmd::Undo), "Removed 1 link");
    assert_eq!(step(&mut s, Cmd::Redo), "Made 1 link again");
    step(&mut s, Cmd::Undo);
    let i = s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].name == "a.txt").unwrap();
    s.cur_mut().select(i);
    step(&mut s, Cmd::Duplicate);
    assert_eq!(step(&mut s, Cmd::Undo), format!("Moved “a copy.txt” to the {bin}"));
    let _ = std::fs::remove_dir_all(&d);
}

/// The column view: with a folder selected (its contents open in the next column), New File and
/// New Folder go into the column holding the selection, the selected folder's parent, as in
/// Finder; into the folder only once its column is the active one.
#[test]
fn in_columns_new_items_go_where_the_selection_is() {
    let d = folder("cols-new-here", 0);
    std::fs::create_dir(d.join("sub")).unwrap();
    let mut s = state_in(&d);
    settle_all(&mut s);
    let mut tx = Tx::default();
    s.run(Cmd::AsColumns, &mut tx);
    settle_all(&mut s);
    let i = s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].name == "sub").unwrap();
    s.cur_mut().select(i);
    s.flush_columns(&mut tx);
    settle_all(&mut s);
    for cmd in [Cmd::NewFile, Cmd::NewFolder] {
        s.rename = None;
        s.run(cmd, &mut tx);
        finish_job(&mut s);
    }
    assert!(d.join("untitled.txt").is_file() && d.join("untitled folder").is_dir(), "beside the selected folder");
    assert_eq!(std::fs::read_dir(d.join("sub")).unwrap().count(), 0, "not inside it");
    let _ = std::fs::remove_dir_all(&d);
}

/// The next launch says what the last quit left, with a way to show it; the record stays
/// until that frame has been shown (a crash before then keeps it for the launch after), then
/// what was read leaves it. An item removed or replaced since isn't mentioned.
#[test]
fn the_next_launch_shows_the_leftovers_then_forgets_them() {
    let d = folder("leftovers", 0);
    std::fs::create_dir_all(d.join("into/payload")).unwrap();
    let file = d.join("leftovers.txt");
    std::fs::write(
        &file,
        format!(
            "{}\t{}\tArbor quit before it could remove it\n{}\tf:1:2:3:4\tgone\n",
            d.join("into/payload").display(),
            crate::ops::mark_of(&d.join("into/payload")).expect("a mark"),
            d.join("nowhere").display()
        ),
    )
    .unwrap();
    let launch = || {
        let (dir, seen) = (d.clone(), crate::leftovers::look(&file));
        let boot = Booting::spawn(Duration::ZERO, move || Start { leftovers: seen, ..settings_for(dir, Settings::default()) });
        let mut s = State::boot(boot, Duration::from_secs(5));
        settle_all(&mut s);
        s
    };
    // A launch that crashes before its first frame is shown: the record is still there.
    let s = launch();
    assert!(s.note.as_deref().is_some_and(|n| n.contains("“payload”")), "{:?}", s.note);
    drop(s);
    assert!(file.exists(), "kept: never shown");
    // The next shows it.
    let mut s = launch();
    let note = s.note.clone().expect("said");
    assert!(note.starts_with("Arbor quit during a file operation and left “payload” partly done in"), "{note}");
    assert!(note.ends_with("Arbor quit before it could remove it"), "{note}");
    assert!(file.exists(), "kept until its frame is on screen");
    s.first_frame_shown();
    let until = Instant::now() + Duration::from_secs(5);
    while file.exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!file.exists(), "forgotten once shown");
    s.show_leftover();
    settle_all(&mut s);
    assert_eq!(s.cur().cwd, d.join("into"));
    assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some("payload"));
    assert!(s.leftover.is_none() && s.note.is_none(), "said once");
}

// ---- the list's columns, Folders First, the status bar --------------------------------------

fn listed(s: &State) -> Vec<String> {
    let f = s.cur();
    f.visible.iter().map(|&i| f.entries[i as usize].name.clone()).collect()
}

/// Columns shown and hidden from the palette (or the header's menu, which runs the same
/// commands), and a header edge dragged: every window draws them, and they're saved (the
/// width when the drag ends, not while it moves; a cancelled drag puts it back, saving
/// nothing). Reset Columns goes back to the defaults.
#[test]
fn columns_and_their_widths_are_saved_for_every_window() {
    use crate::listcols::{self, Field, ListCols};
    use rune::Phase;
    let d = folder("columns", 1);
    let file = d.join("cfg").join(listcols::FILE);
    let mut s = state_in(&d);
    *s.shared.cols_file.borrow_mut() = Some(file.clone());
    let saved = |s: &State| {
        assert!(s.flush_columns_file(Duration::from_secs(5)), "written");
        listcols::load(Some(&file))
    };
    assert_eq!(s.cols(), ListCols::default());
    assert!(s.unavailable(Cmd::ResetColumns).is_some());
    assert_eq!((s.title(Cmd::ColumnCreated), s.checked(Cmd::ColumnCreated)), ("Show the Date Created Column".into(), Some(false)));
    assert_eq!(s.checked(Cmd::Copy), None, "not a toggle");
    let mut other = s.new_window(d.clone());
    other.poll(&mut Tx::default());
    s.run(Cmd::ColumnCreated, &mut Tx::default());
    s.run(Cmd::ColumnKind, &mut Tx::default());
    assert!(s.cols().shown(Field::Created) && !s.cols().shown(Field::Kind));
    assert_eq!(s.note.as_deref(), Some("Kind column hidden"));
    assert_eq!((s.title(Cmd::ColumnCreated), s.checked(Cmd::ColumnCreated)), ("Hide the Date Created Column".into(), Some(true)));
    assert!(other.poll(&mut Tx::default()), "the other window draws them again");
    assert_eq!(other.cols(), s.cols());
    assert_eq!(saved(&s), listcols::Loaded { cols: s.cols(), error: None });
    // Size's edge dragged 100 pt left with 30 to spare: 30 wider, saved when it's let go.
    s.drag_header(Field::Size, Phase::Down, 0.0, 30.0, false);
    s.drag_header(Field::Size, Phase::Move, -100.0, 30.0, false);
    assert_eq!(s.cols().width(Field::Size), 110.0);
    assert_eq!(saved(&s).cols.width(Field::Size), 80.0, "not saved while it moves");
    s.drag_header(Field::Size, Phase::Up, -100.0, 30.0, false);
    assert_eq!(saved(&s).cols.width(Field::Size), 110.0);
    assert_eq!(other.cols().width(Field::Size), 110.0);
    // Dragged right past its least, then Esc: back to 110, nothing saved.
    s.drag_header(Field::Size, Phase::Down, 0.0, 0.0, false);
    s.drag_header(Field::Size, Phase::Move, 500.0, 0.0, false);
    assert_eq!(s.cols().width(Field::Size), Field::Size.min_w());
    s.drag_header(Field::Size, Phase::Up, 500.0, 0.0, true);
    assert_eq!(s.cols().width(Field::Size), 110.0);
    assert_eq!(saved(&s).cols.width(Field::Size), 110.0);
    // The header's menu: every column, checked as shown, then Folders First and Reset.
    let items = s.header_menu_items();
    let titles: Vec<String> = items.iter().map(|i| i.as_ref().map_or("-".into(), |(_, t, _)| t.clone())).collect();
    assert_eq!(
        titles,
        [
            "Kind",
            "Extension",
            "Size",
            "Date Modified",
            "Date Created",
            Field::Permissions.title(),
            "-",
            "Folders First",
            "-",
            "Reset Columns"
        ]
    );
    let checks: Vec<Option<bool>> = items.iter().flatten().map(|(a, _, _)| a.cmd().and_then(|c| s.checked(c))).collect();
    assert_eq!(checks, [Some(false), Some(false), Some(true), Some(true), Some(true), Some(false), Some(true), None]);
    s.run(Cmd::ResetColumns, &mut Tx::default());
    assert_eq!(s.cols(), ListCols::default());
    assert_eq!(saved(&s).cols, ListCols::default());
}

/// Folders First, from the palette: folders mix with files (under the sort, and in every tab,
/// a new one too), and back. The setting starts a window with it off, and Reload Settings
/// changes it without undoing a header click.
#[test]
fn folders_first_turns_off_and_on() {
    let d = folder("folders-first", 0);
    for n in ["b", "d"] {
        std::fs::create_dir(d.join(n)).unwrap();
    }
    for n in ["a.txt", "c.txt"] {
        std::fs::write(d.join(n), "x").unwrap();
    }
    let mut s = state_in(&d);
    settle_all(&mut s);
    assert_eq!(listed(&s), ["b", "d", "a.txt", "c.txt"]);
    assert_eq!(s.title(Cmd::FoldersFirst), "Turn Off Folders First");
    s.run(Cmd::FoldersFirst, &mut Tx::default());
    assert_eq!(listed(&s), ["a.txt", "b", "c.txt", "d"]);
    assert_eq!((s.checked(Cmd::FoldersFirst), s.title(Cmd::FoldersFirst)), (Some(false), "Turn On Folders First".into()));
    s.run(Cmd::SortReverse, &mut Tx::default());
    assert_eq!(listed(&s), ["d", "c.txt", "b", "a.txt"], "under any sort");
    s.new_tab(&mut Tx::default());
    assert!(!s.cur().sort.dirs_first, "a new tab too");
    s.run(Cmd::FoldersFirst, &mut Tx::default());
    assert_eq!(listed(&s), ["d", "b", "c.txt", "a.txt"]);
    // `folders_first = false` in the settings: a window starts with folders mixed in.
    let dir = d.clone();
    let off = Settings { sort: fs::Sort { dirs_first: false, ..Settings::default().sort }, ..Settings::default() };
    let mut s = State::boot(Booting::spawn(Duration::ZERO, move || settings_for(dir, off)), Duration::from_secs(5));
    settle_all(&mut s);
    assert_eq!(listed(&s), ["a.txt", "b", "c.txt", "d"]);
    let cfg = d.join("config.toml");
    s.set_sort(SortKey::Size);
    std::fs::write(&cfg, "[general]\nfolders_first = true\n").unwrap();
    s.reload_settings_from(Some(cfg));
    let until = Instant::now() + Duration::from_secs(10);
    while !s.folders_first && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(2));
        s.poll(&mut Tx::default());
    }
    assert!(s.folders_first && s.cur().sort.dirs_first);
    assert_eq!(s.cur().sort.key, SortKey::Size, "the header click stays");
    // Folders first again (by size, largest first: both 0, so by name the other way).
    assert_eq!(&listed(&s)[..2], ["d", "b"]);
}

/// Two or more selected: how many, the files' total size, and the folders counted apart
/// (their size isn't walked); worked out again when the selection changes.
#[test]
fn the_selections_size_adds_up_its_files() {
    let d = folder("picked-size", 0);
    std::fs::write(d.join("a"), vec![0u8; 1000]).unwrap();
    std::fs::write(d.join("b"), vec![0u8; 2500]).unwrap();
    std::fs::create_dir(d.join("dir")).unwrap();
    std::fs::write(d.join("dir").join("inside"), vec![0u8; 9000]).unwrap();
    let mut s = state_in(&d);
    settle_all(&mut s);
    assert_eq!(s.cur().picked_size(), None, "one item: the row says its size");
    s.cur_mut().select_all();
    let p = s.cur().picked_size().unwrap();
    assert_eq!(p, crate::folder::Picked { count: 3, bytes: 3500, folders: 1 });
    assert_eq!(crate::view::picked_text(p), "3 selected · 3.5 KB + 1 folder");
    let dir = s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].is_dir).unwrap();
    s.cur_mut().toggle_mark(dir);
    let p = s.cur().picked_size().unwrap();
    assert_eq!(crate::view::picked_text(p), "2 selected · 3.5 KB");
    let files = s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].name == "a").unwrap();
    s.cur_mut().toggle_mark(files);
    assert_eq!(s.cur().picked_size(), None, "one left");
    assert_eq!(crate::view::picked_text(crate::folder::Picked { count: 2, bytes: 0, folders: 2 }), "2 selected");
}

/// The volume's free space is read on a worker when a folder is read and after a file
/// operation, once each: never on the window's thread, and never again while idle.
#[test]
fn free_space_is_read_off_the_window_thread_and_not_while_idle() {
    let d = folder("free-space", 1);
    let mut s = state_in(&d);
    settle_all(&mut s);
    let me = std::thread::current().id();
    let reads = || crate::fs::free_reads::of(&d);
    let until = |s: &mut State, done: &dyn Fn(&State) -> bool| {
        let t = Instant::now() + Duration::from_secs(10);
        while !done(s) && Instant::now() < t {
            std::thread::sleep(Duration::from_millis(2));
            s.poll(&mut Tx::default());
        }
    };
    s.reload();
    until(&mut s, &|s| s.cur().free().is_some());
    assert!(s.cur().free().is_some_and(|n| n > 0));
    let after_read = reads();
    assert!(!after_read.is_empty() && !after_read.contains(&me), "on a worker: {after_read:?}");
    // Idle: polls (as wakes from elsewhere would make) read nothing more.
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(2));
        s.poll(&mut Tx::default());
    }
    assert_eq!(reads().len(), after_read.len(), "nothing read while idle");
    // A file operation asks again (here with the folder's reload): a fresh answer comes. (It
    // may be another folder's read on the same disk that answers, so this folder's own reads
    // aren't counted for it.)
    s.cur_mut().free = None;
    s.new_folder();
    finish_job(&mut s);
    until(&mut s, &|s| s.cur().free().is_some());
    assert!(s.cur().free().is_some(), "answered again after the operation");
    let after_op = reads();
    assert!(!after_op.contains(&me), "{after_op:?}");
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(2));
        s.poll(&mut Tx::default());
    }
    assert_eq!(reads().len(), after_op.len(), "nothing read while idle");
}

/// Free-space reads never take the listings' workers: a `statvfs` on a dead network volume
/// never returns and can't be stopped. With every folder's free-space read hung, each folder
/// still lists (they ran on Rune's four shared workers, so four hung reads starved every
/// listing after them).
#[test]
fn hung_free_space_reads_never_hold_up_listings() {
    let _one_at_a_time = crate::volinfo::hanging();
    let dirs: Vec<PathBuf> = (0..7).map(|i| folder(&format!("hung-free-{i}"), 1)).collect();
    let gates: Vec<_> = dirs.iter().map(|d| crate::fs::free_reads::hold(d)).collect();
    let mut s = state_in(&dirs[0]);
    settle_all(&mut s);
    for d in dirs.iter().cycle().skip(1).take(8) {
        s.go(d.clone(), Nav::New, None);
        let t = Instant::now();
        while s.is_loading() && t.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(1));
            s.poll(&mut Tx::default());
        }
        assert!(!s.is_loading(), "{} wasn't listed: free-space reads held up the listing", d.display());
        assert_eq!(&s.cur().cwd, d);
    }
    // All on one volume: one read in flight for it, however many folders asked (it may wait
    // behind another test's read on the same disk, so it's waited for).
    let started = || dirs.iter().map(|d| crate::fs::free_reads::of(d).len()).sum::<usize>();
    let t = Instant::now();
    while started() == 0 && t.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(2));
    }
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(started(), 1, "one hung read per volume, not one per folder");
    assert_eq!(s.cur().free(), None, "nothing known for the folder yet");
    drop(gates);
}

/// A reload that lands after the user picked again keeps their choice, and finding its items
/// in the new listing is done on a worker, never on the window's thread (it looks through the
/// whole folder: 28 ms at 500,000 items).
#[test]
fn a_reload_after_a_new_pick_finds_it_off_the_window_thread() {
    let d = folder("reload-race-worker", 6);
    let mut s = state_in(&d);
    settle_all(&mut s);
    let f = s.cur_mut();
    f.select(0);
    f.toggle_mark(1);
    s.reload();
    // Picked again before it lands: f0, f1 and f4.
    s.cur_mut().toggle_mark(4);
    let want = s.cur().target_paths();
    assert_eq!(want.len(), 3);
    settle_all(&mut s);
    assert_eq!(s.cur().target_paths(), want);
    assert_eq!(s.cur().picked_size().map(|p| p.count), Some(3));
    assert!(!crate::folder::marked_on::here(), "looked through the listing on the window's thread");
}

/// Hiding deselects a hidden item, and that's a change of selection like any other: a reload
/// asked for before (carrying the old selection) doesn't bring it back when it lands, even
/// after the hidden files are shown again.
#[test]
fn a_reload_never_brings_back_an_item_hiding_deselected() {
    let d = folder("hidden-race", 0);
    for n in [".hidden", "a", "b"] {
        std::fs::write(d.join(n), n).unwrap();
    }
    let mut s = state_in(&d);
    settle_all(&mut s);
    s.toggle_hidden();
    s.cur_mut().select_all();
    assert_eq!(s.cur().target_paths().len(), 3);
    s.reload();
    s.toggle_hidden();
    s.toggle_hidden();
    let want = vec![d.join("a"), d.join("b")];
    assert_eq!(s.cur().target_paths(), want, "hiding deselected it");
    settle_all(&mut s);
    assert_eq!(s.cur().target_paths(), want, "the reload didn't bring it back");
}

/// A hidden tab keeps its selection exactly, and its cursor apart: one item selected with the
/// cursor on another (⌘-click on, ⌘-click off) comes back as that item, not the cursor's.
#[test]
fn a_hidden_tab_keeps_its_selection_exactly() {
    let d = folder("memo-exact", 0);
    for n in ["a", "b", "c"] {
        std::fs::write(d.join(n), n).unwrap();
    }
    let mut s = state_in(&d);
    settle_all(&mut s);
    let f = s.cur_mut();
    f.select(0);
    f.toggle_mark(1);
    f.toggle_mark(1);
    let want = vec![d.join("a")];
    assert_eq!(s.cur().target_paths(), want);
    // Hidden and shown again (as a tab switch does), its folder read fresh.
    s.new_tab(&mut Tx::default());
    s.run(Cmd::PrevTab, &mut Tx::default());
    settle_all(&mut s);
    assert_eq!(s.cur().target_paths(), want, "the selection");
    assert_eq!(s.cur().cursor_name().as_deref(), Some(std::ffi::OsStr::new("b")), "the cursor, apart");
}

/// A reload of a big folder with a big selection hands the selection to its worker as it is
/// (shared pointers and a word per 64 rows), never copying its names on the window's thread:
/// asking for it, and landing one that raced a new pick, took 70 ms at 500,000 items. In a
/// release build each is held to 5 ms here at 300,000; the selection that lands is the new
/// pick, whole.
#[test]
fn a_big_reload_hands_its_selection_over_without_copying_names() {
    let d = folder("reload-big", 0);
    let mut s = state_in(&d);
    settle_all(&mut s);
    let n = 300_000;
    let f = s.cur_mut();
    f.entries = (0..n).map(|i| Entry::test(&format!("{i:08}.txt"), false)).collect::<Vec<_>>().into();
    f.refilter(None);
    f.select_all();
    let t = Instant::now();
    s.reload();
    let ask = t.elapsed();
    // A listing of the same items lands after the user picked again (⌘A once more).
    let f = s.cur_mut();
    let req = f.pending_for(d.clone(), Nav::Reload, Vec::new());
    f.select_all();
    let listing = fs::Listing { path: d.clone(), result: Ok(f.entries.to_vec()), dir_id: None, missing: false };
    let a = Answer::new(req, f.sort, listing);
    let t = Instant::now();
    assert!(!s.land(0, a, &mut Tx::default()), "it goes to a worker first");
    let land = t.elapsed();
    settle_all(&mut s);
    assert_eq!(s.cur().targets().len(), n, "the new pick, whole");
    assert!(!crate::folder::marked_on::here(), "names looked through on the window's thread");
    eprintln!("reload asked: {ask:?}; racing land: {land:?}");
    if !cfg!(debug_assertions) {
        assert!(ask < Duration::from_millis(5) && land < Duration::from_millis(5), "asked {ask:?}, landed {land:?}");
    }
}

/// The selection, as identities (names: one folder's are unique), against a plain model of it
/// under random runs of what changes it or the rows it's on: clicks, ⌘-clicks, ⇧-clicks, ⌘A,
/// Invert, sorts, Folders First, hidden files shown and hidden, a filter typed and backspaced,
/// files added, removed and renamed on disk then read again, reloads that race a new pick,
/// and a tab hidden and shown again. A choice (a click, typing) makes the model what was
/// chosen; anything else only takes out what isn't shown any more, and never puts anything
/// in. After every step, what an action would act on is exactly the model.
#[test]
fn the_selection_matches_a_model_whatever_happens() {
    use std::collections::BTreeSet;
    use std::ffi::OsString;
    fn shown(s: &State) -> Vec<OsString> {
        let f = s.cur();
        f.visible.iter().map(|&i| f.entries[i as usize].os.clone()).collect()
    }
    fn targets(s: &State) -> BTreeSet<OsString> {
        s.cur().targets().iter().map(|e| e.os.clone()).collect()
    }
    fn keep_shown(model: &mut BTreeSet<OsString>, s: &State) {
        let rows: BTreeSet<OsString> = shown(s).into_iter().collect();
        model.retain(|n| rows.contains(n));
    }
    let d = folder("selection-model", 0);
    crate::prop::check("selection_model", 24, |r| {
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut made = 0;
        let mut add = |r: &mut crate::prop::Rng| {
            made += 1;
            let dot = if r.one_in(5) { "." } else { "" };
            let name = format!("{dot}{}{made}.{}", ["a", "B", "c", "img_", "Z"][r.below(5)], ["txt", "jpg", "md"][r.below(3)]);
            std::fs::write(d.join(&name), vec![b'x'; r.below(3000)]).unwrap();
            if r.one_in(6) {
                std::fs::create_dir(d.join(format!("dir{made}"))).unwrap();
            }
        };
        for _ in 0..10 {
            add(r);
        }
        let mut s = state_in(&d);
        settle_all(&mut s);
        let mut model: BTreeSet<OsString> = s.cur().selected().map(|e| e.os.clone()).into_iter().collect();
        let mut log = Vec::new();
        for step in 0..40 {
            let n = s.cur().visible.len();
            let row = |r: &mut crate::prop::Rng| r.below(n.max(1));
            let what = r.below(14);
            match what {
                0 | 1 if n > 0 => {
                    let i = row(r);
                    log.push(format!("click {i}"));
                    s.cur_mut().select(i);
                    model = [shown(&s)[i].clone()].into();
                }
                2 | 3 if n > 0 => {
                    let i = row(r);
                    log.push(format!("cmd-click {i}"));
                    s.cur_mut().toggle_mark(i);
                    let name = shown(&s)[i].clone();
                    if !model.remove(&name) {
                        model.insert(name);
                    }
                }
                4 if n > 0 => {
                    let i = row(r);
                    let a = s.cur().anchor.min(n - 1);
                    log.push(format!("shift-click {i} (anchor {a})"));
                    s.cur_mut().extend_to(i);
                    model = shown(&s)[a.min(i)..=a.max(i)].iter().cloned().collect();
                }
                5 => {
                    log.push("select all".into());
                    s.cur_mut().select_all();
                    model = shown(&s).into_iter().collect();
                }
                6 => {
                    log.push("invert".into());
                    s.cur_mut().invert_selection();
                    model = shown(&s).into_iter().filter(|n| !model.contains(n)).collect();
                }
                7 => {
                    let cmd = [Cmd::SortName, Cmd::SortKind, Cmd::SortSize, Cmd::SortModified, Cmd::SortReverse][r.below(5)];
                    log.push(format!("{cmd:?}"));
                    s.run(cmd, &mut Tx::default());
                }
                8 => {
                    log.push("folders first".into());
                    s.run(Cmd::FoldersFirst, &mut Tx::default());
                }
                9 => {
                    log.push("hidden files".into());
                    s.toggle_hidden();
                    keep_shown(&mut model, &s);
                }
                10 => {
                    // Typing chooses the first match; backspacing only shows more.
                    let c = ["a", "1", "z", "img", "q"][r.below(5)];
                    log.push(format!("type {c:?}, backspace"));
                    s.cur_mut().type_filter(c);
                    model = s.cur().cursor_name().into_iter().filter(|_| !s.cur().visible.is_empty()).collect();
                    for _ in 0..c.len() {
                        s.on_key(&KeyEvent { key: Key::Backspace, mods: rune::Mods::default() }, &mut Tx::default());
                    }
                    keep_shown(&mut model, &s);
                }
                11 | 12 => {
                    // The disk changes, then the folder is read again; or a pick races the read.
                    let names: Vec<OsString> = std::fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name()).collect();
                    match r.below(3) {
                        0 => add(r),
                        1 if !names.is_empty() => {
                            let p = d.join(&names[r.below(names.len())]);
                            let _ = std::fs::remove_file(&p).or_else(|_| std::fs::remove_dir(&p));
                        }
                        _ if !names.is_empty() => {
                            let from = &names[r.below(names.len())];
                            let _ = std::fs::rename(d.join(from), d.join(format!("renamed{step}-{}", from.to_string_lossy())));
                        }
                        _ => {}
                    }
                    s.reload();
                    let racing = what == 12 && n > 0;
                    if racing {
                        let i = row(r);
                        s.cur_mut().toggle_mark(i);
                        let name = shown(&s)[i].clone();
                        if !model.remove(&name) {
                            model.insert(name);
                        }
                    }
                    log.push(format!("disk change, reload{}", if racing { " raced by a cmd-click" } else { "" }));
                    settle_all(&mut s);
                    keep_shown(&mut model, &s);
                }
                13 => {
                    log.push("tab hidden and shown".into());
                    s.new_tab(&mut Tx::default());
                    s.run(Cmd::PrevTab, &mut Tx::default());
                    settle_all(&mut s);
                    keep_shown(&mut model, &s);
                    let (ix, last) = (s.active, s.panes[s.active].tabs.len() - 1);
                    s.close_tab(ix, last, &mut Tx::default());
                }
                _ => continue,
            }
            assert_eq!(targets(&s), model, "after {log:#?}\nrows {:?}", shown(&s));
            let p = s.cur().picked_size();
            assert_eq!(
                p.map_or(model.len().min(1), |p| p.count),
                model.len().max(usize::from(p.is_none() && !model.is_empty())),
                "count after {log:#?}"
            );
        }
    });
}

/// A command on "the item" (Rename, Get Info) acts on a selected item: with the cursor
/// ⌘-clicked off and another item still selected, that one, not the cursor's.
#[test]
fn the_item_is_always_a_selected_one() {
    let d = folder("the-item", 0);
    for n in ["a", "b", "c"] {
        std::fs::write(d.join(n), n).unwrap();
    }
    let mut s = state_in(&d);
    settle_all(&mut s);
    let f = s.cur_mut();
    f.select(0);
    f.toggle_mark(1);
    f.toggle_mark(1);
    assert_eq!(s.cur().cursor_name().as_deref(), Some(std::ffi::OsStr::new("b")));
    assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some("a"));
    s.start_rename();
    assert_eq!(s.rename.as_ref().map(|r| r.os.clone()), Some("a".into()), "renaming what's selected");
}

/// A free-space answer still on its way when its window closes is dropped: it never wakes a
/// window that's gone.
#[test]
fn a_closed_window_isnt_woken_by_its_free_space() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let _one_at_a_time = crate::volinfo::hanging();
    let d = folder("free-closed", 1);
    let mut a = state_in(&d);
    settle_all(&mut a);
    let go = crate::fs::free_reads::hold(&d);
    let woken = Arc::new(AtomicUsize::new(0));
    let n = woken.clone();
    a.setup(Arc::new(move || {
        n.fetch_add(1, Ordering::SeqCst);
    }));
    let t = Instant::now();
    while crate::fs::free_reads::of(&d).is_empty() && t.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(2));
    }
    a.leave();
    woken.store(0, Ordering::SeqCst);
    drop(go);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(woken.load(Ordering::SeqCst), 0, "the closed window was woken");
}

/// Typing to select is a choice like a click: a reload asked for before (with the old
/// selection) lands with what typing chose.
#[test]
fn typing_during_a_reload_keeps_what_typing_chose() {
    let d = folder("typing-race", 0);
    for n in ["apple", "banana", "blueberry"] {
        std::fs::write(d.join(n), n).unwrap();
    }
    let mut s = state_in(&d);
    settle_all(&mut s);
    assert_eq!(s.cur().target_paths(), [d.join("apple")]);
    s.reload();
    s.cur_mut().type_filter("b");
    assert_eq!(s.cur().target_paths(), [d.join("banana")]);
    settle_all(&mut s);
    assert_eq!(s.cur().target_paths(), [d.join("banana")], "not apple, nor anything else");
}

/// Starting a rename and cancelling it leaves the selection exactly as it was: one item
/// selected with the cursor ⌘-clicked off another stays that item, not the cursor's.
#[test]
fn a_cancelled_rename_leaves_the_selection_as_it_was() {
    let d = folder("rename-cancel", 0);
    for n in ["a", "b", "c"] {
        std::fs::write(d.join(n), n).unwrap();
    }
    let mut s = state_in(&d);
    settle_all(&mut s);
    let f = s.cur_mut();
    f.select(0);
    f.toggle_mark(1);
    f.toggle_mark(1);
    assert_eq!(s.cur().target_paths(), [d.join("a")]);
    s.start_rename();
    assert_eq!(s.rename.as_ref().map(|r| r.os.clone()), Some("a".into()));
    s.cancel_rename(&mut Tx::default());
    assert_eq!(s.cur().target_paths(), [d.join("a")]);
}
