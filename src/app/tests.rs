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
    Start { config: Config { settings, ..Config::default() }, start: dir, places: Vec::new(), note: None, custom: false }
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
    assert!(s.job.is_some(), "made on the worker, not on the window's thread");
    wait_job(&mut s);
    assert!(s.is_loading(), "the reload is on its way");
    let i = s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].name == "delete-me-1.txt").unwrap();
    s.cur_mut().select(i);
    settle_all(&mut s);
    assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some("delete-me-1.txt"));
    assert!(s.rename.is_none(), "no rename started on a folder the user moved away from");
    s.copy_selected(false, &mut Tx::default());
    assert_eq!(s.clip.as_ref().map(|c| c.paths.clone()), Some(vec![d.join("delete-me-1.txt")]));
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
    while s.job.is_some() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
        s.poll_job();
    }
    assert!(s.job.is_none(), "the operation never finished");
}

/// Polls as frames would until the file operation lands (or 10 s pass).
pub(crate) fn finish_job(s: &mut State) {
    let until = Instant::now() + Duration::from_secs(10);
    while s.job.is_some() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(2));
        s.poll(&mut Tx::default());
    }
    assert!(s.job.is_none(), "the operation never finished");
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
    assert_eq!((s.undo.len(), s.redo.len()), (0, 1));
    assert_eq!(s.unavailable(Cmd::Redo), None);
    s.run(Cmd::Redo, &mut tx);
    finish_job(&mut s);
    assert!(d.join("a copy.txt").exists(), "redone");
    assert_eq!(s.note.as_deref(), Some("Redid 1 item"));
    assert_eq!((s.undo.len(), s.redo.len()), (1, 0));
    // Undo it again, then a new operation: nothing to redo any more.
    s.run(Cmd::Undo, &mut tx);
    finish_job(&mut s);
    assert_eq!(s.redo.len(), 1);
    s.new_folder();
    assert!(s.redo.is_empty(), "a new folder ends the redo");
    finish_job(&mut s);
    s.rename = None;
    s.run(Cmd::Undo, &mut tx);
    finish_job(&mut s);
    assert_eq!(s.redo.len(), 1);
    let i = s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].name == "a.txt").unwrap();
    s.cur_mut().select(i);
    s.run(Cmd::Duplicate, &mut tx);
    assert!(s.redo.is_empty(), "so does a copy");
    finish_job(&mut s);
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
    let late =
        |req: u64, p: &Path| Answer { req, sort: Settings::default().sort, listing: fs::list(p.to_path_buf(), Settings::default().sort) };
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
    assert!(s.job.is_some());
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
    assert!(s.job.is_some(), "on the worker");
    assert_eq!(s.cur().selected().and_then(|e| e.pending.as_deref()), Some("b.txt"), "shown at once");
    finish_job(&mut s);
    assert!(d.join("b.txt").exists() && !d.join("a.txt").exists());
    assert_eq!((s.note.as_deref(), s.undo.len()), (None, 1), "nothing to say; undoable");
    s.new_folder();
    assert!(s.job.is_some(), "on the worker");
    finish_job(&mut s);
    assert!(d.join("untitled folder").is_dir());
    assert!(s.rename.is_some(), "renaming the new folder once it's listed");
    assert_eq!(s.undo.len(), 2);
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
    s.job = Some(ops::start(ops::Op::Copy { sources: Vec::new(), dest: d.clone() }, None, None));
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
    let content = rune::ClipboardContent::new("").with_files(s.clip.as_ref().unwrap().paths.clone());
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
    let content = rune::ClipboardContent::new("").with_files(s.clip.as_ref().unwrap().paths.clone());
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
