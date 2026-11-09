use super::*;
use std::ffi::OsStr;
use std::fs;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("arbor-ops-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Stands in for the platform trash: moves the item to a `.trash` folder beside it, so tests
/// never touch the real one.
/// An item named "notrash…" stands for one on a disk with no trash: it's left alone.
pub(super) fn fake_trash(p: &Path, _stop: &dyn Fn() -> bool) -> Result<Option<PathBuf>, TrashError> {
    if p.file_name().unwrap().to_string_lossy().starts_with("notrash") {
        return Err(TrashError::NoTrash("the test disk has no trash".into()));
    }
    let bin = p.parent().unwrap().join(".trash");
    let failed = |e: io::Error| TrashError::Failed(e.to_string());
    fs::create_dir_all(&bin).map_err(failed)?;
    let at = free_name(&bin, p.file_name().unwrap(), false);
    fs::rename(p, &at).map_err(failed)?;
    Ok(Some(at))
}

fn refusing_trash(_: &Path, _stop: &dyn Fn() -> bool) -> Result<Option<PathBuf>, TrashError> {
    Err(TrashError::Failed("permission denied".into()))
}

fn run_now(op: Op) -> Outcome {
    run_with(op, fake_trash)
}

/// Undoes `u`; panics unless it went through cleanly.
fn undo_ok(u: Undo) {
    let o = run_now(Op::Undo(u));
    assert!(o.refused.is_none() && o.errors.is_empty(), "{:?} {:?}", o.refused, o.errors);
    assert!(o.undo.is_none());
}

/// Undoes `u`; panics unless it was refused, changing nothing, with the record handed back.
/// Returns the message.
fn undo_refused(u: Undo) -> String {
    let steps = u.steps.len();
    let o = run_now(Op::Undo(u));
    let why = o.refused.expect("undo should have been refused");
    assert!(o.results.is_empty());
    assert_eq!(o.undo.map(|u| u.steps.len()), Some(steps), "the whole record goes back on the stack");
    why
}

/// Changes the content (same length) and moves the mtime, as an editor saving would.
fn edit(p: &Path) {
    let len = fs::metadata(p).unwrap().len() as usize;
    fs::write(p, "z".repeat(len)).unwrap();
    bump(p);
}

/// Moves the mtime an hour on, without sleeping or depending on the clock's granularity.
fn bump(p: &Path) {
    let t = fs::metadata(p).unwrap().modified().unwrap() + Duration::from_secs(3600);
    fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
}

fn take(o: Outcome) -> Undo {
    assert!(o.errors.is_empty(), "{:?}", o.errors);
    o.undo.expect("no undo recorded")
}

#[test]
fn copy_into_same_folder_makes_copies() {
    let d = tmp("dup");
    fs::write(d.join("a.txt"), "hi").unwrap();
    let o = run_now(Op::Copy { sources: vec![d.join("a.txt")], dest: d.clone() });
    assert_eq!(o.results, vec![d.join("a copy.txt")]);
    let o = run_now(Op::Copy { sources: vec![d.join("a.txt")], dest: d.clone() });
    assert_eq!(o.results, vec![d.join("a copy 2.txt")]);
    assert_eq!(fs::read_to_string(d.join("a copy 2.txt")).unwrap(), "hi");
}

#[test]
fn copy_tree_keeps_both() {
    let d = tmp("tree");
    fs::create_dir_all(d.join("src/sub")).unwrap();
    fs::write(d.join("src/sub/f"), "x").unwrap();
    fs::create_dir(d.join("dst")).unwrap();
    let o = run_now(Op::Copy { sources: vec![d.join("src")], dest: d.join("dst") });
    assert!(o.errors.is_empty(), "{:?}", o.errors);
    assert_eq!(fs::read_to_string(d.join("dst/src/sub/f")).unwrap(), "x");
    // Copying again elsewhere keeps both.
    let o = run_now(Op::Copy { sources: vec![d.join("src")], dest: d.join("dst") });
    assert_eq!(o.results, vec![d.join("dst/src 2")]);
}

#[test]
fn move_never_overwrites() {
    let d = tmp("mv");
    fs::create_dir(d.join("into")).unwrap();
    fs::write(d.join("f.txt"), "new").unwrap();
    fs::write(d.join("into/f.txt"), "old").unwrap();
    let o = run_now(Op::Move { sources: vec![d.join("f.txt")], dest: d.join("into") });
    assert_eq!(o.results, vec![d.join("into/f 2.txt")]);
    assert_eq!(fs::read_to_string(d.join("into/f.txt")).unwrap(), "old");
}

#[test]
fn refuses_folder_into_itself() {
    let d = tmp("self");
    fs::create_dir_all(d.join("a/b")).unwrap();
    let o = run_now(Op::Move { sources: vec![d.join("a")], dest: d.join("a/b") });
    assert_eq!(o.errors.len(), 1);
    let o = run_now(Op::Copy { sources: vec![d.join("a")], dest: d.join("a/b") });
    assert_eq!(o.errors.len(), 1);
    assert!(!d.join("a/b/a").exists());
}

// ---- redo -------------------------------------------------------------------------------

/// Runs undo record `u` (an undo or a redo); panics unless it went cleanly. Returns the
/// record that reverses it.
fn reverse(u: Undo) -> Undo {
    let redo = u.redo;
    let o = run_now(Op::Undo(u));
    assert!(o.refused.is_none() && o.errors.is_empty(), "{:?} {:?}", o.refused, o.errors);
    assert!(o.undo.is_none(), "nothing left over");
    let inv = o.inverse.expect("an inverse");
    assert_eq!(inv.redo, !redo, "it points the other way");
    inv
}

/// Move, undo, redo, undo: the item goes back and forth, and each record reverses the last.
#[test]
fn redo_a_move_and_undo_it_again() {
    let d = tmp("redo-move");
    fs::create_dir(d.join("into")).unwrap();
    fs::write(d.join("f.txt"), "1").unwrap();
    let u = take(run_now(Op::Move { sources: vec![d.join("f.txt")], dest: d.join("into") }));
    let redo = reverse(u);
    assert!(d.join("f.txt").exists() && !d.join("into/f.txt").exists());
    assert_eq!(Op::Undo(redo.clone()).verb(), "Redoing");
    let undo = reverse(redo);
    assert!(!d.join("f.txt").exists() && d.join("into/f.txt").exists(), "redone");
    reverse(undo);
    assert!(d.join("f.txt").exists(), "undone again");
}

/// A redo refuses, as an undo does, when the item changed since the undo left it, and
/// hands the record back untouched.
#[test]
fn redo_refuses_when_the_disk_changed() {
    let d = tmp("redo-refuse");
    fs::create_dir(d.join("into")).unwrap();
    fs::write(d.join("f.txt"), "1").unwrap();
    let redo = reverse(take(run_now(Op::Move { sources: vec![d.join("f.txt")], dest: d.join("into") })));
    edit(&d.join("f.txt"));
    let o = run_now(Op::Undo(redo));
    let why = o.refused.expect("refused");
    assert!(why.starts_with("Can't redo the move: “f.txt” was edited"), "{why}");
    assert!(o.undo.is_some_and(|u| u.redo), "the redo stays a redo");
    assert!(o.inverse.is_none());
    assert!(d.join("f.txt").exists() && !d.join("into/f.txt").exists(), "nothing moved");
    // Something put where it would go back to: refused too.
    let d = tmp("redo-taken");
    fs::create_dir(d.join("into")).unwrap();
    fs::write(d.join("f.txt"), "1").unwrap();
    let redo = reverse(take(run_now(Op::Move { sources: vec![d.join("f.txt")], dest: d.join("into") })));
    fs::write(d.join("into/f.txt"), "someone else's").unwrap();
    let why = run_now(Op::Undo(redo)).refused.expect("refused");
    assert!(why.contains("already taken"), "{why}");
    assert_eq!(fs::read_to_string(d.join("into/f.txt")).unwrap(), "someone else's");
}

/// Copy, undo (the copy goes to the Trash), redo (it comes back out), undo again.
#[test]
fn redo_a_copy_takes_it_back_out_of_the_trash() {
    let d = tmp("redo-copy");
    fs::create_dir_all(d.join("src")).unwrap();
    fs::write(d.join("src/f"), "x").unwrap();
    fs::create_dir(d.join("dst")).unwrap();
    let u = take(run_now(Op::Copy { sources: vec![d.join("src")], dest: d.join("dst") }));
    let redo = reverse(u);
    assert!(!d.join("dst/src").exists() && d.join("dst/.trash/src/f").exists());
    let undo = reverse(redo);
    assert_eq!(fs::read_to_string(d.join("dst/src/f")).unwrap(), "x", "back where the copy made it");
    // Worked in since: the undo of the redone copy refuses, as the first undo would.
    fs::write(d.join("dst/src/new"), "mine").unwrap();
    let why = undo_refused(undo);
    assert!(why.starts_with("Can't undo the copy: items were added") && why.contains("“src”"), "{why}");
}

/// The review's case: undo a folder's copy, edit a file inside it where it sits in the
/// Trash, then redo. Redo used to take it back out (its record had only the folder's own
/// fingerprint); now it refuses, as undoing a copy does when anything inside changed.
#[test]
fn redo_of_a_copy_refuses_a_folder_edited_in_the_trash() {
    let d = tmp("redo-copy-edited");
    fs::create_dir_all(d.join("src/nested")).unwrap();
    fs::write(d.join("src/nested/f"), "old").unwrap();
    fs::create_dir(d.join("dst")).unwrap();
    let u = take(run_now(Op::Copy { sources: vec![d.join("src")], dest: d.join("dst") }));
    let redo = reverse(u);
    let in_trash = redo.steps[0].at.clone();
    fs::write(in_trash.join("nested/f"), "edited in the trash").unwrap();
    let why = undo_refused(redo);
    assert!(why.contains("“f”") || why.contains("changed") || why.contains("edited"), "{why}");
    assert!(!d.join("dst/src").exists(), "it stays in the Trash");
}

/// Trash, undo (it comes back), redo (it goes to the Trash again), undo (back again).
#[test]
fn redo_a_trash() {
    let d = tmp("redo-trash");
    fs::write(d.join("f"), "1").unwrap();
    let u = take(run_now(Op::Trash { sources: vec![d.join("f")] }));
    let redo = reverse(u);
    assert!(d.join("f").exists());
    let undo = reverse(redo);
    assert!(!d.join("f").exists());
    reverse(undo);
    assert_eq!(fs::read_to_string(d.join("f")).unwrap(), "1");
    // A redo that finds the item gone from the Trash's side refuses.
    let u = take(run_now(Op::Trash { sources: vec![d.join("f")] }));
    let redo = reverse(u);
    let undo = reverse(redo);
    fs::remove_dir_all(d.join(".trash")).unwrap();
    let why = undo_refused(undo);
    assert!(why.contains("no longer in the Trash"), "{why}");
}

/// A batch rename's undo and redo are symmetric: the same plan both ways, swaps included.
#[test]
fn redo_a_batch_rename_with_a_swap() {
    let d = tmp("redo-batch");
    fs::write(d.join("a"), "A").unwrap();
    fs::write(d.join("b"), "B").unwrap();
    let item = |from: &str, to: &str| Renaming { from: d.join(from), to: d.join(to), seen: Seen::of(&d.join(from)).unwrap() };
    let u = take(run_now(Op::Rename { items: vec![item("a", "b"), item("b", "a")] }));
    assert_eq!(fs::read_to_string(d.join("a")).unwrap(), "B");
    let redo = reverse(u);
    assert_eq!(fs::read_to_string(d.join("a")).unwrap(), "A", "undone");
    assert_eq!(redo.what, What::BatchRename);
    let undo = reverse(redo);
    assert_eq!(fs::read_to_string(d.join("a")).unwrap(), "B", "redone");
    assert_eq!(fs::read_to_string(d.join("b")).unwrap(), "A");
    reverse(undo);
    assert_eq!(fs::read_to_string(d.join("a")).unwrap(), "A", "undone again");
}

// ---- undo: rename -----------------------------------------------------------------------

#[test]
fn undo_rename() {
    let d = tmp("ren-ok");
    fs::write(d.join("a"), "1").unwrap();
    fs::rename(d.join("a"), d.join("b")).unwrap();
    undo_ok(Undo::renamed(d.join("b"), d.join("a")).unwrap());
    assert_eq!(fs::read_to_string(d.join("a")).unwrap(), "1");
}

#[test]
fn undo_rename_refuses_when_edited() {
    let d = tmp("ren-edit");
    fs::write(d.join("a"), "1").unwrap();
    fs::rename(d.join("a"), d.join("b")).unwrap();
    let u = Undo::renamed(d.join("b"), d.join("a")).unwrap();
    edit(&d.join("b"));
    let why = undo_refused(u);
    assert!(why.contains("“b” was edited"), "{why}");
    assert!(d.join("b").exists() && !d.join("a").exists());
}

#[test]
fn undo_rename_refuses_when_replaced() {
    let d = tmp("ren-repl");
    fs::write(d.join("a"), "1").unwrap();
    fs::rename(d.join("a"), d.join("b")).unwrap();
    let u = Undo::renamed(d.join("b"), d.join("a")).unwrap();
    // Same name, size and mtime, but another file.
    let t = fs::metadata(d.join("b")).unwrap().modified().unwrap();
    fs::write(d.join("c"), "2").unwrap();
    fs::File::options().write(true).open(d.join("c")).unwrap().set_modified(t).unwrap();
    fs::remove_file(d.join("b")).unwrap();
    fs::rename(d.join("c"), d.join("b")).unwrap();
    let why = undo_refused(u);
    assert!(why.contains("replaced by another item"), "{why}");
    assert_eq!(fs::read_to_string(d.join("b")).unwrap(), "2");
}

#[test]
fn undo_rename_case_only() {
    let d = tmp("ren-case");
    fs::write(d.join("a.txt"), "1").unwrap();
    fs::rename(d.join("a.txt"), d.join("A.txt")).unwrap();
    // On a case-insensitive disk, "a.txt" still finds the file itself: that isn't taken.
    undo_ok(Undo::renamed(d.join("A.txt"), d.join("a.txt")).unwrap());
    assert!(fs::read_dir(&d).unwrap().flatten().any(|e| e.file_name() == "a.txt"));
}

// ---- undo: move -------------------------------------------------------------------------

#[test]
fn undo_move() {
    let d = tmp("mv-ok");
    fs::create_dir(d.join("into")).unwrap();
    fs::write(d.join("f.txt"), "new").unwrap();
    let u = take(run_now(Op::Move { sources: vec![d.join("f.txt")], dest: d.join("into") }));
    assert_eq!(u.what, What::Move);
    undo_ok(u);
    assert_eq!(fs::read_to_string(d.join("f.txt")).unwrap(), "new");
}

#[test]
fn undo_move_refuses_when_deleted() {
    let d = tmp("mv-del");
    fs::create_dir(d.join("into")).unwrap();
    fs::write(d.join("f.txt"), "x").unwrap();
    let u = take(run_now(Op::Move { sources: vec![d.join("f.txt")], dest: d.join("into") }));
    fs::remove_file(d.join("into/f.txt")).unwrap();
    let why = undo_refused(u);
    assert!(why.contains("moved or deleted"), "{why}");
}

#[test]
fn undo_move_refuses_when_its_spot_is_taken() {
    let d = tmp("mv-taken");
    fs::create_dir(d.join("into")).unwrap();
    fs::write(d.join("f.txt"), "x").unwrap();
    let u = take(run_now(Op::Move { sources: vec![d.join("f.txt")], dest: d.join("into") }));
    fs::write(d.join("f.txt"), "someone else's").unwrap();
    let why = undo_refused(u);
    assert!(why.contains("already taken"), "{why}");
    assert_eq!(fs::read_to_string(d.join("f.txt")).unwrap(), "someone else's");
}

/// One changed item stops the whole undo: the unchanged one stays put too.
#[test]
fn undo_is_all_or_nothing() {
    let d = tmp("mv-all");
    fs::create_dir(d.join("into")).unwrap();
    fs::write(d.join("a"), "a").unwrap();
    fs::write(d.join("b"), "b").unwrap();
    let u = take(run_now(Op::Move { sources: vec![d.join("a"), d.join("b")], dest: d.join("into") }));
    edit(&d.join("into/b"));
    let why = undo_refused(u);
    assert!(why.starts_with("Can't undo the move: “b” was edited"), "{why}");
    assert!(d.join("into/a").exists() && !d.join("a").exists());
}

/// A folder moved back carries its contents, so moves check only the folder itself.
#[test]
fn undo_move_of_folder_with_an_edit_deep_inside() {
    let d = tmp("mv-deep");
    fs::create_dir_all(d.join("f/sub")).unwrap();
    fs::write(d.join("f/sub/x"), "x").unwrap();
    fs::create_dir(d.join("into")).unwrap();
    let u = take(run_now(Op::Move { sources: vec![d.join("f")], dest: d.join("into") }));
    edit(&d.join("into/f/sub/x"));
    undo_ok(u);
    assert_eq!(fs::read_to_string(d.join("f/sub/x")).unwrap(), "z");
}

// ---- undo: copy and duplicate -----------------------------------------------------------

#[test]
fn undo_copy() {
    let d = tmp("cp-ok");
    fs::create_dir(d.join("to")).unwrap();
    fs::write(d.join("a"), "a").unwrap();
    let u = take(run_now(Op::Copy { sources: vec![d.join("a")], dest: d.join("to") }));
    assert_eq!(u.what, What::Copy);
    undo_ok(u);
    assert!(!d.join("to/a").exists() && d.join("to/.trash/a").exists() && d.join("a").exists());
}

#[test]
fn undo_copy_refuses_when_edited() {
    let d = tmp("cp-edit");
    fs::create_dir(d.join("to")).unwrap();
    fs::write(d.join("a"), "a").unwrap();
    let u = take(run_now(Op::Copy { sources: vec![d.join("a")], dest: d.join("to") }));
    edit(&d.join("to/a"));
    let why = undo_refused(u);
    assert!(why.starts_with("Can't undo the copy: “a” was edited"), "{why}");
    assert!(d.join("to/a").exists());
}

#[test]
fn undo_copy_of_folder() {
    let d = tmp("cpdir-ok");
    fs::create_dir_all(d.join("src/sub")).unwrap();
    fs::write(d.join("src/sub/f"), "x").unwrap();
    fs::create_dir(d.join("dst")).unwrap();
    let u = take(run_now(Op::Copy { sources: vec![d.join("src")], dest: d.join("dst") }));
    assert_eq!(u.steps[0].print.tree.map(|t| t.items), Some(2));
    undo_ok(u);
    assert!(!d.join("dst/src").exists());
}

/// An edit deep inside a copied folder doesn't move the folder's own mtime; the walk sees it.
#[test]
fn undo_copy_of_folder_refuses_when_edited_deep_inside() {
    let d = tmp("cpdir-edit");
    fs::create_dir_all(d.join("src/sub")).unwrap();
    fs::write(d.join("src/sub/f"), "x").unwrap();
    fs::create_dir(d.join("dst")).unwrap();
    let u = take(run_now(Op::Copy { sources: vec![d.join("src")], dest: d.join("dst") }));
    bump(&d.join("dst/src/sub/f"));
    let why = undo_refused(u);
    assert!(why.contains("something inside “src” was edited"), "{why}");
    assert!(d.join("dst/src/sub/f").exists());
}

#[test]
fn undo_copy_of_folder_refuses_when_added_to_deep_inside() {
    let d = tmp("cpdir-add");
    fs::create_dir_all(d.join("src/sub")).unwrap();
    fs::create_dir(d.join("dst")).unwrap();
    let u = take(run_now(Op::Copy { sources: vec![d.join("src")], dest: d.join("dst") }));
    fs::write(d.join("dst/src/sub/new"), "mine").unwrap();
    let why = undo_refused(u);
    assert!(why.contains("added inside “src”"), "{why}");
}

#[test]
fn undo_duplicate() {
    let d = tmp("dup-ok");
    fs::write(d.join("a.txt"), "a").unwrap();
    let u = take(run_now(Op::Copy { sources: vec![d.join("a.txt")], dest: d.clone() }));
    assert_eq!(u.what, What::Duplicate);
    undo_ok(u);
    assert!(!d.join("a copy.txt").exists() && d.join("a.txt").exists());
}

#[test]
fn undo_duplicate_refuses_when_replaced_by_a_folder() {
    let d = tmp("dup-repl");
    fs::write(d.join("a.txt"), "a").unwrap();
    let u = take(run_now(Op::Copy { sources: vec![d.join("a.txt")], dest: d.clone() }));
    fs::remove_file(d.join("a copy.txt")).unwrap();
    fs::create_dir(d.join("a copy.txt")).unwrap();
    let why = undo_refused(u);
    assert!(why.contains("replaced by a folder"), "{why}");
}

// ---- undo: new folder -------------------------------------------------------------------

#[test]
fn undo_new_folder() {
    let d = tmp("nf-ok");
    fs::create_dir(d.join("untitled folder")).unwrap();
    undo_ok(Undo::new_folder(d.join("untitled folder")).unwrap());
    assert!(!d.join("untitled folder").exists());
}

#[test]
fn undo_new_folder_refuses_once_used() {
    let d = tmp("nf-used");
    fs::create_dir(d.join("untitled folder")).unwrap();
    let u = Undo::new_folder(d.join("untitled folder")).unwrap();
    fs::write(d.join("untitled folder/notes.txt"), "keep me").unwrap();
    let why = undo_refused(u);
    assert!(why.starts_with("Can't undo the new folder:"), "{why}");
    assert!(d.join("untitled folder/notes.txt").exists());
}

// ---- undo: trash ------------------------------------------------------------------------

#[test]
fn undo_trash() {
    let d = tmp("tr-ok");
    fs::write(d.join("a"), "a").unwrap();
    let u = take(run_now(Op::Trash { sources: vec![d.join("a")] }));
    assert!(!d.join("a").exists());
    undo_ok(u);
    assert_eq!(fs::read_to_string(d.join("a")).unwrap(), "a");
}

#[test]
fn undo_trash_refuses_once_gone_from_the_trash() {
    let d = tmp("tr-gone");
    fs::write(d.join("a"), "a").unwrap();
    let u = take(run_now(Op::Trash { sources: vec![d.join("a")] }));
    fs::remove_file(d.join(".trash/a")).unwrap(); // emptied
    let why = undo_refused(u);
    assert!(why.contains("no longer in the Trash"), "{why}");
}

#[test]
fn undo_trash_refuses_when_its_spot_is_taken() {
    let d = tmp("tr-taken");
    fs::write(d.join("a"), "a").unwrap();
    let u = take(run_now(Op::Trash { sources: vec![d.join("a")] }));
    fs::write(d.join("a"), "new a").unwrap();
    let why = undo_refused(u);
    assert!(why.contains("already taken"), "{why}");
    assert_eq!(fs::read_to_string(d.join("a")).unwrap(), "new a");
}

// ---- undo: failures keep the record ------------------------------------------------------

/// A trash that fails (a declined prompt, a permission error) leaves the whole record, so
/// the undo can be tried again.
#[test]
fn failed_undo_keeps_its_record() {
    let d = tmp("fail-keep");
    fs::write(d.join("a"), "a").unwrap();
    let u = take(run_now(Op::Copy { sources: vec![d.join("a")], dest: d.clone() }));
    let o = run_with(Op::Undo(u), refusing_trash);
    assert_eq!(o.errors.len(), 1);
    let u = o.undo.expect("record kept");
    assert_eq!(u.steps.len(), 1);
    undo_ok(u); // and it can be retried
    assert!(!d.join("a copy").exists());
}

/// Trashes like `fake_trash`, but refuses anything named "b…".
fn picky_trash(p: &Path, stop: &dyn Fn() -> bool) -> Result<Option<PathBuf>, TrashError> {
    if p.file_name().unwrap().to_string_lossy().starts_with('b') {
        Err(TrashError::Failed("permission denied".into()))
    } else {
        fake_trash(p, stop)
    }
}

/// A partial undo keeps a record of only what didn't go back, so a retry never repeats the
/// part that worked.
#[test]
fn partial_undo_keeps_only_the_rest() {
    let d = tmp("fail-part");
    fs::create_dir(d.join("to")).unwrap();
    fs::write(d.join("a"), "a").unwrap();
    fs::write(d.join("b"), "b").unwrap();
    let u = take(run_now(Op::Copy { sources: vec![d.join("a"), d.join("b")], dest: d.join("to") }));
    let o = run_with(Op::Undo(u), picky_trash);
    assert!(o.refused.is_none());
    assert_eq!(o.results, vec![d.join("to/a")]);
    assert_eq!(o.errors.len(), 1);
    let left = o.undo.expect("the rest is kept");
    assert_eq!(left.steps.iter().map(|s| s.at.clone()).collect::<Vec<_>>(), vec![d.join("to/b")]);
    undo_ok(left);
    assert!(!d.join("to/b").exists() && d.join("to/.trash/a").exists());
}

// ---- never overwrite, even when a name is taken after the check ---------------------------

/// A hook that puts another app's file at the first destination it's shown.
fn intruder() -> PathHook {
    let mut done = false;
    Box::new(move |dest: &Path| {
        if !std::mem::replace(&mut done, true) {
            fs::write(dest, "intruder").unwrap();
        }
    })
}

fn hooked(before_act: PathHook, cross_device: bool) -> Hooks {
    Hooks { before_act: Some(before_act), cross_device, ..Hooks::default() }
}

#[test]
fn rename_never_overwrites_a_name_taken_after_the_check() {
    let d = tmp("race-ren");
    fs::write(d.join("a"), "a").unwrap();
    let e = rename_between(&d.join("a"), &d.join("b"), || fs::write(d.join("b"), "intruder").unwrap()).unwrap_err();
    assert_eq!(e, "“b” already exists");
    assert_eq!(fs::read_to_string(d.join("b")).unwrap(), "intruder");
    assert_eq!(fs::read_to_string(d.join("a")).unwrap(), "a");
}

#[test]
fn move_never_overwrites_a_name_taken_after_it_was_chosen() {
    for cross_device in [false, true] {
        let d = tmp(&format!("race-mv-{cross_device}"));
        fs::create_dir(d.join("into")).unwrap();
        fs::write(d.join("f.txt"), "mine").unwrap();
        let op = Op::Move { sources: vec![d.join("f.txt")], dest: d.join("into") };
        let o = run_hooked(op, fake_trash, hooked(intruder(), cross_device));
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(o.results, vec![d.join("into/f 2.txt")], "cross-device: {cross_device}");
        assert_eq!(fs::read_to_string(d.join("into/f.txt")).unwrap(), "intruder");
        assert_eq!(fs::read_to_string(d.join("into/f 2.txt")).unwrap(), "mine");
        assert!(!d.join("f.txt").exists());
    }
}

#[test]
fn copy_never_overwrites_a_name_taken_after_it_was_chosen() {
    for cross_device in [false, true] {
        let d = tmp(&format!("race-cp-{cross_device}"));
        fs::create_dir(d.join("to")).unwrap();
        fs::write(d.join("a.txt"), "mine").unwrap();
        fs::create_dir_all(d.join("dir/sub")).unwrap();
        fs::write(d.join("dir/sub/x"), "x").unwrap();
        let op = Op::Copy { sources: vec![d.join("a.txt"), d.join("dir")], dest: d.join("to") };
        // Another app takes each name the copy picks first: a file, then a folder's name.
        let hook = Box::new(move |dest: &Path| {
            if [OsStr::new("a.txt"), OsStr::new("dir")].contains(&dest.file_name().unwrap()) {
                fs::write(dest, "intruder").unwrap();
            }
        });
        let o = run_hooked(op, fake_trash, hooked(hook, cross_device));
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(o.results, vec![d.join("to/a 2.txt"), d.join("to/dir 2")]);
        assert_eq!(fs::read_to_string(d.join("to/a.txt")).unwrap(), "intruder");
        assert_eq!(fs::read_to_string(d.join("to/dir")).unwrap(), "intruder");
        assert_eq!(fs::read_to_string(d.join("to/a 2.txt")).unwrap(), "mine");
        assert_eq!(fs::read_to_string(d.join("to/dir 2/sub/x")).unwrap(), "x");
    }
    // A duplicate in the same folder too.
    let d = tmp("race-dup");
    fs::write(d.join("a.txt"), "mine").unwrap();
    let o = run_hooked(Op::Copy { sources: vec![d.join("a.txt")], dest: d.clone() }, fake_trash, hooked(intruder(), false));
    assert_eq!(o.results, vec![d.join("a copy 2.txt")]);
    assert_eq!(fs::read_to_string(d.join("a copy.txt")).unwrap(), "intruder");
}

/// The undo passes its check, then another app takes the spot before the move back: that
/// item stays put, its record stays on the stack, and the newcomer is untouched.
#[test]
fn undo_never_overwrites_a_spot_taken_after_the_check() {
    for cross_device in [false, true] {
        let d = tmp(&format!("race-undo-{cross_device}"));
        fs::create_dir(d.join("into")).unwrap();
        fs::write(d.join("f.txt"), "mine").unwrap();
        let u = take(run_now(Op::Move { sources: vec![d.join("f.txt")], dest: d.join("into") }));
        let o = run_hooked(Op::Undo(u), fake_trash, hooked(intruder(), cross_device));
        assert!(o.refused.is_none());
        assert_eq!(o.errors.len(), 1);
        assert!(o.errors[0].contains("was taken just now, so “f.txt” stayed where it is"), "{:?}", o.errors);
        assert_eq!(o.undo.map(|u| u.steps.len()), Some(1));
        assert_eq!(fs::read_to_string(d.join("f.txt")).unwrap(), "intruder");
        assert_eq!(fs::read_to_string(d.join("into/f.txt")).unwrap(), "mine");
    }
}

/// A trash undo too: the spot it goes back to is taken after the check.
#[test]
fn undo_trash_never_overwrites_a_spot_taken_after_the_check() {
    let d = tmp("race-undo-tr");
    fs::write(d.join("a"), "mine").unwrap();
    let u = take(run_now(Op::Trash { sources: vec![d.join("a")] }));
    let o = run_hooked(Op::Undo(u), fake_trash, hooked(intruder(), false));
    assert_eq!(o.errors.len(), 1, "{:?}", o.errors);
    assert_eq!(fs::read_to_string(d.join("a")).unwrap(), "intruder");
    assert_eq!(fs::read_to_string(d.join(".trash/a")).unwrap(), "mine");
}

/// Each step is checked again right before it runs: an item edited while an earlier step
/// was being undone stays put.
#[test]
fn undo_rechecks_each_step_right_before_it() {
    let d = tmp("recheck");
    fs::create_dir(d.join("into")).unwrap();
    fs::write(d.join("a"), "a").unwrap();
    fs::write(d.join("b"), "b").unwrap();
    let u = take(run_now(Op::Move { sources: vec![d.join("a"), d.join("b")], dest: d.join("into") }));
    let b = d.join("into/b");
    let mut once = false;
    let hook = Box::new(move |_: &Path| {
        if !std::mem::replace(&mut once, true) {
            edit(&b);
        }
    });
    let o = run_hooked(Op::Undo(u), fake_trash, hooked(hook, false));
    assert!(o.refused.is_none());
    assert_eq!(o.results, vec![d.join("a")]);
    assert_eq!(o.errors, vec!["Not undone: “b” was edited".to_string()]);
    let left = o.undo.expect("b's step is kept");
    assert_eq!(left.steps.iter().map(|s| s.at.clone()).collect::<Vec<_>>(), vec![d.join("into/b")]);
    assert!(d.join("into/b").exists() && !d.join("b").exists());
}

// ---- cross-disk moves (copy, then delete) -------------------------------------------------

fn across(fail_remove: Option<PathTest>) -> Hooks {
    Hooks { cross_device: true, fail_remove, ..Hooks::default() }
}

#[test]
fn move_across_disks() {
    let d = tmp("x-mv");
    fs::create_dir_all(d.join("f/sub")).unwrap();
    fs::write(d.join("f/sub/x"), "x").unwrap();
    fs::create_dir(d.join("into")).unwrap();
    let o = run_hooked(Op::Move { sources: vec![d.join("f")], dest: d.join("into") }, fake_trash, across(None));
    let u = take(o);
    assert!(!d.join("f").exists());
    assert_eq!(fs::read_to_string(d.join("into/f/sub/x")).unwrap(), "x");
    let o = run_hooked(Op::Undo(u), fake_trash, across(None));
    assert!(o.errors.is_empty() && o.undo.is_none(), "{:?}", o.errors);
    assert!(!d.join("into/f").exists());
    assert_eq!(fs::read_to_string(d.join("f/sub/x")).unwrap(), "x");
}

/// Moves `f` (holding `x` and `y`) into `into`, then undoes it across disks with removing
/// `into/f/y` failing: everything is back in `f`, `y` is left in `into/f`, and the record
/// says so. Returns the dir and the record.
fn split_undo(name: &str) -> (PathBuf, Undo) {
    let d = tmp(name);
    fs::create_dir(d.join("f")).unwrap();
    fs::write(d.join("f/x"), "x").unwrap();
    fs::write(d.join("f/y"), "y").unwrap();
    fs::create_dir(d.join("into")).unwrap();
    let u = take(run_now(Op::Move { sources: vec![d.join("f")], dest: d.join("into") }));
    let stuck = d.join("into/f/y");
    let o = run_hooked(Op::Undo(u), fake_trash, across(Some(Box::new(move |p: &Path| p == stuck))));
    assert!(o.refused.is_none());
    assert_eq!(o.errors.len(), 1);
    let msg = &o.errors[0];
    assert!(msg.starts_with("“f” is back in “") && msg.contains("but part of the original couldn't be removed from “"), "{msg}");
    assert!(msg.contains(&d.join("into").display().to_string()), "{msg}");
    // Nothing lost: all of it is back, and the rest is where it was.
    assert_eq!(fs::read_to_string(d.join("f/x")).unwrap(), "x");
    assert_eq!(fs::read_to_string(d.join("f/y")).unwrap(), "y");
    assert_eq!(fs::read_to_string(d.join("into/f/y")).unwrap(), "y");
    assert!(!d.join("into/f/x").exists());
    let u = o.undo.expect("the step is kept");
    assert!(u.steps[0].split.is_some());
    (d, u)
}

#[test]
fn split_undo_across_disks_finishes_on_retry() {
    let (d, u) = split_undo("x-split-ok");
    undo_ok(u);
    assert!(!d.join("into/f").exists());
    assert_eq!(fs::read_to_string(d.join("f/x")).unwrap(), "x");
    assert_eq!(fs::read_to_string(d.join("f/y")).unwrap(), "y");
}

#[test]
fn split_undo_refuses_when_the_rest_changed() {
    let (d, u) = split_undo("x-split-rest");
    edit(&d.join("into/f/y"));
    let why = undo_refused(u);
    assert!(why.contains("“f” is back in “") && why.contains("what's left of the original in “"), "{why}");
    assert_eq!(fs::read_to_string(d.join("into/f/y")).unwrap(), "z");
    assert_eq!(fs::read_to_string(d.join("f/y")).unwrap(), "y");
}

#[test]
fn split_undo_refuses_when_the_copy_changed() {
    let (d, u) = split_undo("x-split-copy");
    fs::write(d.join("f/new"), "mine").unwrap();
    let why = undo_refused(u);
    assert!(why.contains("was copied back to “") && why.contains("that copy changed since"), "{why}");
    assert!(d.join("into/f/y").exists() && d.join("f/new").exists());
}

/// The rest removed by hand since: the retry has nothing left to do, and finishes.
#[test]
fn split_undo_finishes_when_the_rest_is_gone() {
    let (d, u) = split_undo("x-split-gone");
    fs::remove_dir_all(d.join("into/f")).unwrap();
    undo_ok(u);
    assert_eq!(fs::read_to_string(d.join("f/y")).unwrap(), "y");
}

/// A forward move across disks whose delete fails says where the copy is, and keeps the rest.
#[test]
fn move_across_disks_that_cant_remove_the_original_says_where_it_is() {
    let d = tmp("x-mv-split");
    fs::create_dir(d.join("f")).unwrap();
    fs::write(d.join("f/x"), "x").unwrap();
    fs::create_dir(d.join("into")).unwrap();
    let stuck = d.join("f/x");
    let op = Op::Move { sources: vec![d.join("f")], dest: d.join("into") };
    let o = run_hooked(op, fake_trash, across(Some(Box::new(move |p: &Path| p == stuck))));
    assert_eq!(o.errors.len(), 1);
    assert!(o.errors[0].contains(&format!("copied to “{}”", d.join("into").join("f").display())), "{:?}", o.errors);
    assert_eq!(fs::read_to_string(d.join("into/f/x")).unwrap(), "x");
    assert_eq!(fs::read_to_string(d.join("f/x")).unwrap(), "x");
}

/// The rest of a split step can't be read when the undo is tried again (its folder lost its
/// permissions): that isn't "gone". The retry refuses, keeps the record naming both places,
/// and finishes once the folder can be read again.
#[cfg(unix)]
#[test]
fn split_retry_keeps_the_record_while_the_rest_cant_be_read() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return; // root reads through any permissions
    }
    let lock = |p: &Path, mode| fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
    let (d, u) = split_undo("x-split-locked");
    lock(&d.join("into"), 0o000);
    let o = run_now(Op::Undo(u));
    lock(&d.join("into"), 0o755);
    let why = o.refused.expect("refused while the rest can't be read");
    assert!(why.contains("what's left of the original in “") && why.contains("can't be checked"), "{why}");
    assert!(why.contains(&d.join("into").display().to_string()) && why.contains(&format!("back in “{}”", d.display())), "{why}");
    let u = o.undo.expect("the record is kept");
    assert!(u.steps[0].split.is_some());
    assert_eq!(fs::read_to_string(d.join("into/f/y")).unwrap(), "y");
    undo_ok(u);
    assert!(!d.join("into/f").exists());
}

/// The same, with the folder locked after the retry's check, right before it removes the
/// rest: the removal can't see the rest, so it doesn't count it as removed.
#[cfg(unix)]
#[test]
fn split_retry_that_cant_reach_the_rest_keeps_the_record() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let (d, u) = split_undo("x-split-locked-late");
    let into = d.join("into");
    let hook = Box::new(move |_: &Path| fs::set_permissions(&into, fs::Permissions::from_mode(0o000)).unwrap());
    let o = run_hooked(Op::Undo(u), fake_trash, hooked(hook, false));
    fs::set_permissions(d.join("into"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(o.errors.len(), 1, "{:?}", o.errors);
    assert!(o.errors[0].contains("part of the original couldn't be removed from “"), "{:?}", o.errors);
    assert!(o.undo.is_some_and(|u| u.steps[0].split.is_some()), "the record is kept");
    assert_eq!(fs::read_to_string(d.join("into/f/y")).unwrap(), "y");
}

// ---- cleaning up a failed or cancelled copy ----------------------------------------------

/// A copy (and a move across disks, which copies first) is cancelled after another app
/// replaced the second file it made. Cleanup removes the first file, which is still Arbor's,
/// keeps the replacement and the folder holding it, says so, and leaves the source alone.
#[test]
fn cancelled_copy_cleanup_keeps_what_another_app_replaced() {
    for cross_device in [false, true] {
        let d = tmp(&format!("unmake-{cross_device}"));
        fs::create_dir_all(d.join("src")).unwrap();
        for n in ["a", "b", "c"] {
            fs::write(d.join("src").join(n), n).unwrap();
        }
        fs::create_dir(d.join("into")).unwrap();
        let p = Arc::new(Progress::default());
        let (pp, dd) = (p.clone(), d.clone());
        let mut files = Vec::new();
        let hook = Box::new(move |dst: &Path| {
            if dst.is_dir() {
                return;
            }
            files.push(dst.to_path_buf());
            if files.len() == 2 {
                fs::rename(dst, dd.join("saved")).unwrap();
                fs::write(dst, "OTHER APP DATA").unwrap();
                pp.cancel.store(true, Relaxed);
            }
        });
        let mut w = Worker::new(&p, None, fake_trash);
        w.hooks = Hooks { after_made: Some(hook), cross_device, ..Hooks::default() };
        let op = if cross_device {
            Op::Move { sources: vec![d.join("src")], dest: d.join("into") }
        } else {
            Op::Copy { sources: vec![d.join("src")], dest: d.join("into") }
        };
        let o = w.run(op);
        assert!(o.cancelled && o.undo.is_none() && o.results.is_empty());
        let left: Vec<_> = fs::read_dir(d.join("into/src")).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(left.len(), 1, "only the replacement is left: {left:?}");
        assert_eq!(fs::read_to_string(&left[0]).unwrap(), "OTHER APP DATA");
        let name = left[0].file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(o.errors.len(), 1, "{:?}", o.errors);
        let want = format!("src: cancelled; kept “{name}” in “{}”: it changed after Arbor made it", d.join("into").join("src").display());
        assert_eq!(o.errors[0], want);
        for n in ["a", "b", "c"] {
            assert_eq!(fs::read_to_string(d.join("src").join(n)).unwrap(), n, "the source is untouched");
        }
    }
}

/// Another app moves a file aside and puts its own at the name while Arbor is still
/// writing it (a copy stalls as long as its source does), then the copy is cancelled. The
/// file Arbor made is known by the handle that wrote it, so cleanup leaves the other app's
/// file alone and says so. With a move across disks, which copies first, the same.
#[test]
fn a_file_replaced_while_it_was_copied_is_never_taken_for_arbors() {
    for cross_device in [false, true] {
        let d = tmp(&format!("replaced-mid-copy-{cross_device}"));
        fs::create_dir_all(d.join("src")).unwrap();
        for n in ["a", "b"] {
            fs::write(d.join("src").join(n), n).unwrap();
        }
        fs::create_dir(d.join("into")).unwrap();
        let p = Arc::new(Progress::default());
        let (pp, dd) = (p.clone(), d.clone());
        let hook = Box::new(move |dst: &Path| {
            if !dd.join("aside").exists() {
                fs::rename(dst, dd.join("aside")).unwrap();
                fs::write(dst, "OTHER APP DATA").unwrap();
                pp.cancel.store(true, Relaxed);
            }
        });
        let mut w = Worker::new(&p, None, fake_trash);
        w.hooks = Hooks { copied: Some(hook), cross_device, ..Hooks::default() };
        let op = if cross_device {
            Op::Move { sources: vec![d.join("src")], dest: d.join("into") }
        } else {
            Op::Copy { sources: vec![d.join("src")], dest: d.join("into") }
        };
        let o = w.run(op);
        assert!(o.cancelled && o.results.is_empty(), "{:?}", o.errors);
        let left: Vec<_> = fs::read_dir(d.join("into/src")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left.len(), 1, "{left:?}");
        assert_eq!(fs::read_to_string(d.join("into/src").join(&left[0])).unwrap(), "OTHER APP DATA", "never removed");
        assert!(o.errors[0].contains("it changed after Arbor made it"), "and said: {:?}", o.errors);
        for n in ["a", "b"] {
            assert_eq!(fs::read_to_string(d.join("src").join(n)).unwrap(), n, "the source is untouched");
        }
    }
}

/// The reviewer's case, for real: the source is a FIFO, so the copy stalls inside the byte
/// copy until something writes to it. Meanwhile another app moves Arbor's half-written file
/// aside and puts its own at the name. The copy's record is of the file Arbor wrote, so
/// undo refuses instead of sending the other app's file to the Trash.
#[cfg(unix)]
#[test]
fn a_file_replaced_during_a_stalled_copy_is_never_taken_for_arbors() {
    use std::io::Write;
    let d = tmp("stalled-copy");
    let into = d.join("into");
    fs::create_dir(&into).unwrap();
    let gate = d.join("gate");
    let c = std::ffi::CString::new(gate.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let p = Arc::new(Progress::default());
    let pp = p.clone();
    let dst = into.join("gate");
    let op = Op::Copy { sources: vec![gate.clone()], dest: into.clone() };
    let worker = std::thread::spawn(move || Worker::new(&pp, None, fake_trash).run(op));
    // Opening the FIFO's writing end waits for the copy to open its reading end; the copy
    // makes its file next, then waits for bytes.
    let mut writer = fs::OpenOptions::new().write(true).open(&gate).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while !dst.exists() && Instant::now() < until {
        std::thread::yield_now();
    }
    fs::rename(&dst, d.join("aside")).unwrap();
    fs::write(&dst, "OTHER APP DATA").unwrap();
    writer.write_all(b"gate").unwrap();
    drop(writer);
    let o = worker.join().unwrap();
    assert_eq!(o.results, std::slice::from_ref(&dst), "{:?}", o.errors);
    assert_eq!(fs::read_to_string(d.join("aside")).unwrap(), "gate", "what Arbor wrote went where the other app put it");
    let u = run_now(Op::Undo(o.undo.unwrap()));
    assert!(u.refused.is_some(), "the file at the name isn't the one Arbor made");
    assert_eq!(fs::read_to_string(&dst).unwrap(), "OTHER APP DATA", "never trashed");
}

/// A copied folder replaced by another before the copy finished isn't recorded as the copy.
#[test]
fn a_folder_replaced_during_its_copy_is_not_recorded_as_the_copy() {
    let d = tmp("replaced-folder");
    fs::create_dir_all(d.join("src")).unwrap();
    fs::write(d.join("src/a"), "a").unwrap();
    fs::create_dir(d.join("into")).unwrap();
    let p = Progress::default();
    let dd = d.clone();
    let hook = Box::new(move |dst: &Path| {
        // The folder's only file is in: swap the folder for another.
        let top = dst.parent().unwrap();
        fs::rename(top, dd.join("aside")).unwrap();
        fs::create_dir(top).unwrap();
        fs::write(top.join("a"), "theirs").unwrap();
    });
    let mut w = Worker::new(&p, None, fake_trash);
    w.hooks = Hooks { copied: Some(hook), ..Hooks::default() };
    let o = w.run(Op::Copy { sources: vec![d.join("src")], dest: d.join("into") });
    assert!(o.undo.is_none() && o.results.is_empty());
    assert!(o.errors[0].contains("something else took the place of “src”"), "{:?}", o.errors);
    assert_eq!(fs::read_to_string(d.join("into/src/a")).unwrap(), "theirs", "the other folder stays, with what's in it");
}

// ---- batch rename -----------------------------------------------------------------------

/// A batch rename of `pairs` (names in `d`), fingerprinted as they are now (the preview).
fn batch(d: &Path, pairs: &[(&str, &str)]) -> Op {
    let items =
        pairs.iter().map(|(a, b)| Renaming { from: d.join(a), to: d.join(b), seen: Seen::of(&d.join(a)).expect("exists") }).collect();
    Op::Rename { items }
}

fn files(d: &Path, names: &[&str]) {
    for n in names {
        fs::write(d.join(n), format!("I was {n}")).unwrap();
    }
}

/// What's in `d`, name → contents ("I was a"), skipping the fake trash.
fn contents(d: &Path) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = fs::read_dir(d)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name() != ".trash")
        .map(|e| (e.file_name().to_string_lossy().into_owned(), fs::read_to_string(e.path()).unwrap_or_default()))
        .collect();
    v.sort();
    v
}

fn was(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = pairs.iter().map(|(n, w)| (n.to_string(), format!("I was {w}"))).collect();
    v.sort();
    v
}

#[test]
fn batch_rename_swaps_chains_and_cycles() {
    let d = tmp("batch-swap");
    files(&d, &["a", "b", "c", "x", "y", "n"]);
    let o = run_now(batch(&d, &[("a", "b"), ("b", "a"), ("x", "y"), ("y", "z"), ("c", "n2"), ("n", "c")]));
    assert!(o.errors.is_empty() && o.refused.is_none(), "{:?} {:?}", o.errors, o.refused);
    assert_eq!(contents(&d), was(&[("a", "b"), ("b", "a"), ("y", "x"), ("z", "y"), ("n2", "c"), ("c", "n")]));
    assert_eq!(o.results.len(), 6);
    assert_eq!(o.moved.len(), 6);
    // A three-way cycle.
    let d = tmp("batch-cycle");
    files(&d, &["1", "2", "3"]);
    take(run_now(batch(&d, &[("1", "2"), ("2", "3"), ("3", "1")])));
    assert_eq!(contents(&d), was(&[("2", "1"), ("3", "2"), ("1", "3")]));
}

#[test]
fn batch_rename_changes_case_only() {
    let d = tmp("batch-case");
    files(&d, &["a.txt", "b.txt"]);
    take(run_now(batch(&d, &[("a.txt", "A.txt"), ("b.txt", "B.TXT")])));
    assert_eq!(contents(&d), was(&[("A.txt", "a.txt"), ("B.TXT", "b.txt")]));
}

/// One undo entry undoes the whole batch, swaps included (going back is a swap too).
#[test]
fn batch_rename_is_one_undo_entry() {
    let d = tmp("batch-undo");
    files(&d, &["a", "b", "c"]);
    let u = take(run_now(batch(&d, &[("a", "b"), ("b", "a"), ("c", "d")])));
    assert_eq!(u.what, What::BatchRename);
    assert_eq!(u.steps.len(), 3);
    let o = run_now(Op::Undo(u));
    assert!(o.errors.is_empty() && o.refused.is_none() && o.undo.is_none(), "{:?} {:?}", o.errors, o.refused);
    assert_eq!(contents(&d), was(&[("a", "a"), ("b", "b"), ("c", "c")]));
    assert_eq!(o.results.len(), 3);
}

/// Undo refuses as a whole when one item changed since, and keeps the record.
#[test]
fn batch_rename_undo_refuses_when_one_item_changed() {
    let d = tmp("batch-undo-edit");
    files(&d, &["a", "b"]);
    let u = take(run_now(batch(&d, &[("a", "b"), ("b", "a")])));
    edit(&d.join("a"));
    let why = undo_refused(u);
    assert!(why.starts_with("Can't undo the batch rename: “a” was edited"), "{why}");
    assert_eq!(fs::read_to_string(d.join("b")).unwrap(), "I was a", "nothing went back");
    // Or when an old name was taken by something outside the batch.
    let d = tmp("batch-undo-taken");
    files(&d, &["a"]);
    let u = take(run_now(batch(&d, &[("a", "b")])));
    fs::write(d.join("a"), "newcomer").unwrap();
    let why = undo_refused(u);
    assert!(why.contains("“a” already exists"), "{why}");
    assert_eq!(fs::read_to_string(d.join("a")).unwrap(), "newcomer");
}

/// An item changed after the preview: nothing is renamed, and the message names it.
#[test]
fn batch_rename_refuses_items_changed_since_the_preview() {
    let d = tmp("batch-changed");
    files(&d, &["a", "b"]);
    let op = batch(&d, &[("a", "x"), ("b", "y")]);
    edit(&d.join("b"));
    let o = run_now(op);
    assert_eq!(o.refused.as_deref(), Some("Nothing was renamed: “b” was edited since the preview."));
    assert!(o.undo.is_none() && o.results.is_empty());
    assert_eq!(contents(&d).iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    // Replaced by another file of the same name.
    let op = batch(&d, &[("a", "x")]);
    fs::remove_file(d.join("a")).unwrap();
    fs::write(d.join("a"), "I was a").unwrap();
    let o = run_now(op);
    assert!(o.refused.as_deref().is_some_and(|r| r.contains("replaced") || r.contains("edited")), "{:?}", o.refused);
    // A new name taken since the preview by something outside the batch.
    let op = batch(&d, &[("a", "x"), ("b", "y")]);
    fs::write(d.join("y"), "newcomer").unwrap();
    let o = run_now(op);
    assert!(o.refused.as_deref().is_some_and(|r| r.contains("“y” already exists")), "{:?}", o.refused);
    assert_eq!(fs::read_to_string(d.join("y")).unwrap(), "newcomer");
    assert!(d.join("a").exists() && !d.join("x").exists());
}

/// The third rename fails (a permission error): the first two, and the swap's temporary
/// name, go back, and nothing is left under a temporary name.
#[test]
fn batch_rename_rolls_back_a_failure_midway() {
    let d = tmp("batch-fail");
    files(&d, &["a", "b", "c", "d"]);
    let stop = d.join("d2");
    let hooks = Hooks { fail_rename: Some(Box::new(move |p: &Path| p == stop)), ..Hooks::default() };
    let o = run_hooked(batch(&d, &[("a", "b"), ("b", "a"), ("c", "c2"), ("d", "d2")]), fake_trash, hooks);
    assert!(o.undo.is_none() && o.results.is_empty() && o.moved.is_empty());
    assert_eq!(o.errors.len(), 1, "{:?}", o.errors);
    assert_eq!(o.errors[0], "Couldn't rename “d” to “d2”: permission denied. Nothing was renamed: every item has its old name again");
    assert_eq!(contents(&d), was(&[("a", "a"), ("b", "b"), ("c", "c"), ("d", "d")]));
}

/// The rollback itself meets a name another app took: that item stays where it is, the rest
/// go back, nothing is overwritten, and the message says where the item is.
#[test]
fn batch_rename_that_cant_roll_back_says_where_each_item_is() {
    let d = tmp("batch-stuck");
    files(&d, &["a", "b"]);
    let (stop, old_a) = (d.join("b2"), d.join("a"));
    let hooks = Hooks {
        fail_rename: Some(Box::new(move |p: &Path| p == stop)),
        // Called before each step, and before each step back: another app takes "a" just
        // as it would go back.
        before_act: Some(Box::new(move |p: &Path| {
            if p == old_a {
                fs::write(p, "newcomer").unwrap();
            }
        })),
        ..Hooks::default()
    };
    let o = run_hooked(batch(&d, &[("a", "a2"), ("b", "b2")]), fake_trash, hooks);
    assert_eq!(o.errors.len(), 1, "{:?}", o.errors);
    assert_eq!(
        o.errors[0],
        format!(
            "Couldn't rename “b” to “b2”: permission denied. 1 of 2 items have their old names again; “a” is now “a2” in “{}” (something else took its old name)",
            d.display()
        )
    );
    assert_eq!(contents(&d), vec![("a".into(), "newcomer".into()), ("a2".into(), "I was a".into()), ("b".into(), "I was b".into())]);
}

/// Cancelled midway: everything goes back.
#[test]
fn batch_rename_cancelled_midway_puts_everything_back() {
    let d = tmp("batch-cancel");
    files(&d, &["a", "b", "c"]);
    let p = Arc::new(Progress::default());
    let pp = p.clone();
    let mut n = 0;
    let mut w = Worker::new(&p, None, fake_trash);
    w.hooks.before_act = Some(Box::new(move |_: &Path| {
        n += 1;
        if n == 2 {
            pp.cancel.store(true, Relaxed);
        }
    }));
    let o = w.run(batch(&d, &[("a", "b"), ("b", "c"), ("c", "d")]));
    assert!(o.cancelled && o.undo.is_none());
    assert_eq!(o.errors, ["Cancelled. Nothing was renamed: every item has its old name again"]);
    assert_eq!(contents(&d), was(&[("a", "a"), ("b", "b"), ("c", "c")]));
}

/// Search results: items from several folders, each renamed in its own.
#[test]
fn batch_rename_across_folders() {
    let d = tmp("batch-folders");
    for sub in ["one", "two"] {
        fs::create_dir(d.join(sub)).unwrap();
        fs::write(d.join(sub).join("report.txt"), sub).unwrap();
    }
    let op = batch(&d, &[("one/report.txt", "one/1.txt"), ("two/report.txt", "two/1.txt")]);
    let u = take(run_now(op));
    assert_eq!(fs::read_to_string(d.join("one/1.txt")).unwrap(), "one");
    assert_eq!(fs::read_to_string(d.join("two/1.txt")).unwrap(), "two");
    undo_ok(u);
    assert_eq!(fs::read_to_string(d.join("two/report.txt")).unwrap(), "two");
    // Out of its folder: refused.
    let o = run_now(batch(&d, &[("one/report.txt", "two/x.txt")]));
    assert!(o.refused.as_deref().is_some_and(|r| r.contains("would leave its folder")), "{:?}", o.refused);
}

// ---- known limits -----------------------------------------------------------------------

/// Pins a documented limit (see [`Print`]): contents aren't hashed, so a same-size edit that
/// leaves the mtime where it was (as one inside the disk's timestamp granularity does) isn't
/// seen, and the undo goes through. If this starts failing, the check got stronger: update
/// the docs.
#[test]
fn same_size_edit_within_mtime_granularity_is_not_seen() {
    let d = tmp("coarse");
    fs::write(d.join("a"), "1").unwrap();
    fs::rename(d.join("a"), d.join("b")).unwrap();
    let u = Undo::renamed(d.join("b"), d.join("a")).unwrap();
    let t = fs::metadata(d.join("b")).unwrap().modified().unwrap();
    fs::write(d.join("b"), "2").unwrap();
    fs::File::options().write(true).open(d.join("b")).unwrap().set_modified(t).unwrap();
    undo_ok(u);
    assert_eq!(fs::read_to_string(d.join("a")).unwrap(), "2");
}

// ---- permanent delete and disks with no trash --------------------------------------------

fn doomed(paths: &[PathBuf]) -> Vec<Doomed> {
    paths.iter().map(|p| Doomed::now(p)).collect()
}

/// A folder goes with everything in it, a link inside it goes without its target, and
/// nothing goes on the undo stack.
#[test]
fn delete_removes_folders_whole_and_records_no_undo() {
    let d = tmp("del");
    fs::create_dir_all(d.join("dir/sub")).unwrap();
    fs::write(d.join("dir/sub/x"), "x").unwrap();
    fs::create_dir(d.join("outside")).unwrap();
    fs::write(d.join("outside/keep"), "keep").unwrap();
    let _ = make_symlink(&d.join("outside"), &d.join("dir/link"), true);
    fs::write(d.join("a"), "a").unwrap();
    let o = run_now(Op::Delete { items: doomed(&[d.join("a"), d.join("dir")]) });
    assert!(o.errors.is_empty() && !o.cancelled, "{:?}", o.errors);
    assert_eq!(o.removed, [d.join("a"), d.join("dir")]);
    assert!(o.undo.is_none() && o.results.is_empty());
    assert!(!d.join("a").exists() && !d.join("dir").exists());
    assert_eq!(fs::read_to_string(d.join("outside/keep")).unwrap(), "keep");
    assert_eq!(o.folders.iter().map(|(p, _)| p.as_path()).collect::<Vec<_>>(), [d.as_path()]);
}

/// Each item is checked right before it goes: one replaced (or removed) while an earlier one
/// was being deleted is left alone and said so; the rest go.
#[test]
fn delete_rechecks_each_item_right_before_it_goes() {
    let d = tmp("del-race");
    for n in ["a", "b", "c"] {
        fs::write(d.join(n), n).unwrap();
    }
    let items = doomed(&[d.join("a"), d.join("b"), d.join("c")]);
    // While "a" is being deleted, another app replaces "b" and removes "c".
    let (b, c) = (d.join("b"), d.join("c"));
    let mut once = false;
    let hook: PathHook = Box::new(move |at: &Path| {
        if at.ends_with("a") && !std::mem::replace(&mut once, true) {
            // Made while the old one still exists, so it can't reuse its inode.
            fs::write(b.with_extension("new"), "replacement").unwrap();
            fs::rename(b.with_extension("new"), &b).unwrap();
            // NTFS hands a file renamed onto a name the old one's creation time (tunnelling).
            #[cfg(windows)]
            {
                use std::os::windows::fs::FileTimesExt;
                let later = SystemTime::now() + Duration::from_secs(3600);
                let f = fs::File::options().write(true).open(&b).unwrap();
                f.set_times(fs::FileTimes::new().set_created(later)).unwrap();
            }
            fs::remove_file(&c).unwrap();
        }
    });
    let o = run_hooked(Op::Delete { items }, fake_trash, Hooks { before_act: Some(hook), ..Hooks::default() });
    assert_eq!(o.removed, [d.join("a")]);
    assert_eq!(fs::read_to_string(d.join("b")).unwrap(), "replacement", "the replacement is never deleted");
    assert_eq!(o.errors.len(), 2, "{:?}", o.errors);
    assert!(o.errors[0].contains("“b” isn't the item you chose"), "{}", o.errors[0]);
    assert!(o.errors[1].contains("“c” was already gone"), "{}", o.errors[1]);
}

/// Esc midway: what's gone stays gone (nothing can bring it back), the folder it stopped in
/// is said to be partly deleted, and later items are untouched.
#[test]
fn delete_cancelled_midway_keeps_what_it_removed() {
    let d = tmp("del-cancel");
    fs::create_dir(d.join("big")).unwrap();
    for i in 0..20 {
        fs::write(d.join(format!("big/{i:02}")), "x").unwrap();
    }
    fs::write(d.join("first"), "x").unwrap();
    fs::write(d.join("later"), "x").unwrap();
    let items = doomed(&[d.join("first"), d.join("big"), d.join("later")]);
    let stop: PathTest = Box::new(|at: &Path| at.ends_with("big/10"));
    let o = run_hooked(Op::Delete { items }, fake_trash, Hooks { cancel_at: Some(stop), ..Hooks::default() });
    assert!(o.cancelled);
    assert_eq!(o.removed, [d.join("first")]);
    assert_eq!(o.partly.as_deref(), Some(d.join("big").as_path()));
    let left = fs::read_dir(d.join("big")).unwrap().count();
    assert!(left > 0 && left < 20, "{left} left");
    assert!(d.join("later").exists() && !d.join("first").exists());
}

/// A failure partway through a folder says so truthfully: partly deleted, where it stopped
/// and why, how many items went and how many are left (it used to say "the rest of it was
/// deleted" while leaving more).
#[cfg(unix)]
#[test]
fn a_delete_that_stops_partway_says_what_it_left() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid can't fail.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: permissions don't stop root");
        return;
    }
    let d = tmp("del-partly");
    // doomed/x/y/file: the file goes (y can be changed), then y can't (x can't be changed).
    fs::create_dir_all(d.join("doomed/x/y")).unwrap();
    fs::write(d.join("doomed/x/y/file"), "x").unwrap();
    fs::set_permissions(d.join("doomed/x"), fs::Permissions::from_mode(0o555)).unwrap();
    let o = run_now(Op::Delete { items: doomed(&[d.join("doomed")]) });
    fs::set_permissions(d.join("doomed/x"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(o.removed.is_empty());
    assert_eq!(o.errors.len(), 1, "{:?}", o.errors);
    let e = &o.errors[0];
    assert!(e.starts_with("Partly deleted “doomed”: stopped at “doomed/x/y” (Permission denied"), "{e}");
    assert!(e.ends_with("); 1 item deleted, 3 left"), "{e}");
    assert!(!d.join("doomed/x/y/file").exists() && d.join("doomed/x/y").is_dir());
}

#[test]
fn what_a_stopped_delete_left_is_exact_or_says_it_isnt() {
    use crate::delete::Left;
    let item = Path::new("/x/doomed");
    assert_eq!(left_note(Some(Left::Exactly(3)), item), ", 3 left");
    assert_eq!(
        left_note(Some(Left::Unknown(PathBuf::from("/x/doomed/z"))), item),
        ", some items left (count unknown: couldn't read “doomed/z”)"
    );
    assert_eq!(left_note(None, item), "");
}

/// A permanent delete never crosses into a mounted disk: it stops at the mount point, names
/// it, and leaves what's in it; the folders above it stay, and the counts say so.
#[test]
fn delete_stops_at_a_mount_point() {
    let d = tmp("del-mount");
    fs::create_dir_all(d.join("doomed/mnt")).unwrap();
    fs::write(d.join("doomed/mnt/precious"), "keep").unwrap();
    let canon = fs::canonicalize(&d).unwrap();
    let mounts = crate::mounts::Mounts::of(&[&canon.join("doomed").join("mnt")]);
    let o = run_hooked(Op::Delete { items: doomed(&[d.join("doomed")]) }, fake_trash, Hooks { mounts: Some(mounts), ..Hooks::default() });
    assert!(o.removed.is_empty());
    assert_eq!(o.errors, [format!("Skipped “{}”: another disk is mounted there", Path::new("doomed").join("mnt").display())]);
    assert_eq!(fs::read_to_string(d.join("doomed/mnt/precious")).unwrap(), "keep");
}

/// A cross-disk move removes the originals it copied, but never those on a disk mounted
/// inside the moved folder: they stay (the move is split and says why).
#[test]
fn a_cross_disk_move_leaves_mounted_originals() {
    let d = tmp("mv-mount");
    fs::create_dir_all(d.join("src/mnt")).unwrap();
    fs::write(d.join("src/mnt/precious"), "keep").unwrap();
    fs::write(d.join("src/plain"), "x").unwrap();
    fs::create_dir(d.join("into")).unwrap();
    let canon = fs::canonicalize(&d).unwrap();
    let mounts = crate::mounts::Mounts::of(&[&canon.join("src").join("mnt")]);
    let o = run_hooked(
        Op::Move { sources: vec![d.join("src")], dest: d.join("into") },
        fake_trash,
        Hooks { cross_device: true, mounts: Some(mounts), ..Hooks::default() },
    );
    assert_eq!(fs::read_to_string(d.join("src/mnt/precious")).unwrap(), "keep", "the mounted original stays");
    assert!(!d.join("src/plain").exists(), "the rest moved");
    assert!(d.join("into/src/mnt/precious").exists() && d.join("into/src/plain").exists());
    assert!(o.errors.iter().any(|e| e.contains("another disk is mounted there")), "{:?}", o.errors);
}

/// A trash of items some of which can't go to the trash: the others go (and can be undone);
/// the rest stay where they were, each with its identity, for one question.
#[test]
fn trash_leaves_what_its_disk_cant_take_for_one_question() {
    let d = tmp("no-trash");
    for n in ["a", "notrash-b", "c", "notrash-d"] {
        fs::write(d.join(n), n).unwrap();
    }
    fs::create_dir(d.join("notrash-dir")).unwrap();
    let sources: Vec<PathBuf> = ["a", "notrash-b", "c", "notrash-d", "notrash-dir"].iter().map(|n| d.join(n)).collect();
    let o = run_now(Op::Trash { sources });
    assert!(o.errors.is_empty(), "{:?}", o.errors);
    assert_eq!(o.removed, [d.join("a"), d.join("c")]);
    assert_eq!(o.no_trash, doomed(&[d.join("notrash-b"), d.join("notrash-d"), d.join("notrash-dir")]));
    assert!(o.no_trash.iter().all(|x| x.path.exists() && x.id.is_some()));
    assert!(o.no_trash[2].dir && !o.no_trash[0].dir);
    assert_eq!(o.no_trash_why.as_deref(), Some("the test disk has no trash"));
    assert_eq!(o.undo.as_ref().map(|u| u.steps.len()), Some(2), "the two that went can be undone");
    undo_ok(o.undo.unwrap());
    assert!(d.join("a").exists() && d.join("c").exists());
}

// ---- each item checked right before it's acted on ----------------------------------------

/// Replaces `p` with other contents the way another app's save would: the new file is made
/// while the old one still exists (so it can't reuse its inode), and on Windows given a
/// creation time of its own (NTFS would hand it the old one's).
fn swap_in(p: &Path, contents: &str) {
    let new = p.with_extension("incoming");
    fs::write(&new, contents).unwrap();
    fs::rename(p, p.with_extension("original")).unwrap();
    fs::rename(&new, p).unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTimesExt;
        let f = fs::File::options().write(true).open(p).unwrap();
        f.set_times(fs::FileTimes::new().set_created(SystemTime::now() + Duration::from_secs(3600))).unwrap();
    }
}

/// The review's case: "b" is replaced while "a" is renamed. "b" is checked right before its
/// own step, so the replacement is never renamed; the batch is put back, and says why.
#[test]
fn batch_rename_never_renames_an_item_replaced_midway() {
    let d = tmp("batch-midway");
    files(&d, &["a", "b"]);
    let root = d.clone();
    let hooks = Hooks {
        before_act: Some(Box::new(move |p: &Path| {
            if p == root.join("a2") {
                swap_in(&root.join("b"), "replacement");
            }
        })),
        ..Hooks::default()
    };
    let o = run_hooked(batch(&d, &[("a", "a2"), ("b", "b2")]), fake_trash, hooks);
    assert!(o.undo.is_none() && o.results.is_empty());
    assert_eq!(o.errors.len(), 1, "{:?}", o.errors);
    assert!(o.errors[0].contains("“b”") && o.errors[0].contains("while the batch was renaming"), "{}", o.errors[0]);
    assert!(o.errors[0].ends_with("Nothing was renamed: every item has its old name again"), "{}", o.errors[0]);
    assert_eq!(fs::read_to_string(d.join("b")).unwrap(), "replacement", "never renamed");
    assert_eq!(fs::read_to_string(d.join("a")).unwrap(), "I was a");
    assert!(!d.join("b2").exists() && !d.join("a2").exists());
}

/// The review's case: an item edited while an undo of a batch rename runs isn't moved; the
/// undo is put back and stays on the stack.
#[test]
fn batch_undo_never_moves_an_item_edited_midway() {
    let d = tmp("batch-undo-midway");
    files(&d, &["a", "b"]);
    let u = take(run_now(batch(&d, &[("a", "a2"), ("b", "b2")])));
    let root = d.clone();
    let hooks = Hooks {
        before_act: Some(Box::new(move |p: &Path| {
            if p == root.join("a") {
                edit(&root.join("b2"));
            }
        })),
        ..Hooks::default()
    };
    let o = run_hooked(Op::Undo(u), fake_trash, hooks);
    assert!(o.errors.first().is_some_and(|e| e.contains("“b2”") && e.contains("while the batch was renaming")), "{:?}", o.errors);
    assert!(d.join("b2").exists() && !d.join("b").exists(), "the edited item stays");
    assert!(d.join("a2").exists() && !d.join("a").exists(), "and the batch is put back");
    assert!(o.undo.is_some(), "the undo stays on the stack");
}

/// The review's case: a rollback never moves a replacement into an item's old name, and
/// says truthfully what came back and what didn't.
#[test]
fn batch_rollback_never_puts_a_replacement_back() {
    let d = tmp("batch-rollback-swap");
    files(&d, &["a", "b"]);
    let root = d.clone();
    let stop = d.join("b2");
    let hooks = Hooks {
        fail_rename: Some(Box::new(move |p: &Path| p == stop)),
        before_act: Some(Box::new(move |p: &Path| {
            // While "b" is about to be renamed (after "a" became "a2"), "a2" is replaced.
            if p == root.join("b2") {
                swap_in(&root.join("a2"), "replacement");
            }
        })),
        ..Hooks::default()
    };
    let o = run_hooked(batch(&d, &[("a", "a2"), ("b", "b2")]), fake_trash, hooks);
    assert_eq!(o.errors.len(), 1, "{:?}", o.errors);
    assert_eq!(
        o.errors[0],
        format!(
            "Couldn't rename “b” to “b2”: permission denied. 1 of 2 items have their old names again; “a2” in “{}” changed after it was renamed (it was “a”), so it was left there",
            d.display()
        )
    );
    assert!(!d.join("a").exists(), "the replacement isn't put in a's place");
    assert_eq!(fs::read_to_string(d.join("a2")).unwrap(), "replacement");
    assert_eq!(fs::read_to_string(d.join("b")).unwrap(), "I was b");
}

/// Stands in for a trash that, while trashing "a", lets another app replace "b".
fn trash_then_replace_b(p: &Path, stop: &dyn Fn() -> bool) -> Result<Option<PathBuf>, TrashError> {
    let out = fake_trash(p, stop)?;
    if p.file_name().unwrap() == "a" {
        swap_in(&p.parent().unwrap().join("b"), "replacement");
    }
    Ok(out)
}

/// The review's case, for search results (identities from the walk) and a plain selection
/// (identities taken when the operation begins): "b" replaced while "a" is trashed is
/// checked right before its own turn and left alone.
#[test]
fn trash_rechecks_each_item_right_before_it_goes() {
    for search in [true, false] {
        let d = tmp(if search { "recheck-search" } else { "recheck-plain" });
        files(&d, &["a", "b", "c"]);
        let paths: Vec<PathBuf> = ["a", "b", "c"].iter().map(|n| d.join(n)).collect();
        let op = Op::Trash { sources: paths.clone() };
        let op = if search {
            let expect = paths.iter().map(|p| (p.clone(), crate::fs::ident(&p.symlink_metadata().unwrap()).unwrap())).collect();
            Op::Checked { expect, op: Box::new(op) }
        } else {
            op
        };
        let o = run_with(op, trash_then_replace_b);
        assert_eq!(fs::read_to_string(d.join("b")).unwrap(), "replacement", "search {search}: never trashed");
        assert!(d.join(".trash/a").exists() && d.join(".trash/c").exists() && !d.join(".trash/b").exists());
        assert_eq!(o.removed, [d.join("a"), d.join("c")]);
        let says = if search { "isn't the item the search found" } else { "was replaced while the operation ran" };
        assert!(o.errors.len() == 1 && o.errors[0].contains(&format!("“b” {says}")), "{:?}", o.errors);
        assert_eq!(o.undo.map(|u| u.steps.len()), Some(2), "only what went is undoable");
    }
}

/// A move and a copy check each source right before it too.
#[test]
fn move_and_copy_recheck_each_item_right_before_it_goes() {
    for mv in [true, false] {
        let d = tmp(if mv { "recheck-mv" } else { "recheck-cp" });
        files(&d, &["a", "b"]);
        fs::create_dir(d.join("into")).unwrap();
        let root = d.clone();
        let hooks = Hooks {
            before_act: Some(Box::new(move |p: &Path| {
                if p == root.join("into/a") {
                    swap_in(&root.join("b"), "replacement");
                }
            })),
            ..Hooks::default()
        };
        let sources = vec![d.join("a"), d.join("b")];
        let op = if mv { Op::Move { sources, dest: d.join("into") } } else { Op::Copy { sources, dest: d.join("into") } };
        let o = run_hooked(op, fake_trash, hooks);
        assert!(d.join("into/a").exists() && !d.join("into/b").exists(), "move {mv}");
        assert_eq!(fs::read_to_string(d.join("b")).unwrap(), "replacement");
        assert!(o.errors.len() == 1 && o.errors[0].contains("“b” was replaced while the operation ran"), "{:?}", o.errors);
    }
}

/// The review's case at the worker: a folder and something inside it, in either order, are
/// refused before anything is renamed (the child first used to rename both and then lose
/// the child's undo step).
#[test]
fn batch_rename_refuses_a_folder_and_something_inside_it() {
    let d = tmp("batch-nested");
    fs::create_dir_all(d.join("parent/child")).unwrap();
    fs::write(d.join("parent/child/payload"), "precious").unwrap();
    for pairs in
        [[("parent", "renamed"), ("parent/child", "parent/newchild")], [("parent/child", "parent/newchild"), ("parent", "renamed")]]
    {
        let o = run_now(batch(&d, &pairs));
        let why = o.refused.expect("refused");
        assert!(why.contains("“child” is inside “parent”, which is being renamed too"), "{why}");
        assert!(o.undo.is_none());
        assert!(d.join("parent/child/payload").exists() && !d.join("renamed").exists());
    }
}

/// An item that can't be read right after a batch rename can't be undone, and the outcome
/// says so instead of leaving it out of the undo silently.
#[test]
fn an_item_that_cant_be_recorded_for_undo_is_said() {
    let d = tmp("record-or-say");
    files(&d, &["a"]);
    let mut errors = Vec::new();
    let u = Undo::record_or_say(What::BatchRename, [(d.join("a"), Some(d.join("x"))), (d.join("gone"), Some(d.join("y")))], &mut errors);
    assert_eq!(u.map(|u| u.steps.len()), Some(1));
    assert!(errors.len() == 1 && errors[0].starts_with("“gone” can't be undone"), "{errors:?}");
}

/// A freedesktop.org trash on a "disk" at `top`: `files/` and `info/`, with an item at each
/// name whose record says it came from `from` (relative to `top`, or absolute).
/// The trash folder at `root` (a fixture's), as Empty Trash lists it.
fn trash_bin(root: &Path) -> crate::trash::contents::Bin {
    crate::trash::contents::Bin::freedesktop(root, crate::trash::contents::me())
}

fn fixture_trash(top: &Path, items: &[(&str, &str)]) -> PathBuf {
    let root = top.join(".Trash-1000");
    fs::create_dir_all(root.join("files")).unwrap();
    fs::create_dir_all(root.join("info")).unwrap();
    for (name, from) in items {
        match name.strip_suffix('/') {
            Some(dir) => {
                fs::create_dir_all(root.join("files").join(dir).join("inner")).unwrap();
                fs::write(root.join("files").join(dir).join("inner/f"), "x").unwrap();
            }
            None => fs::write(root.join("files").join(name), *name).unwrap(),
        }
        let n = name.trim_end_matches('/');
        fs::write(
            root.join("info").join(format!("{n}.trashinfo")),
            format!("[Trash Info]\nPath={from}\nDeletionDate=2026-11-09T10:00:00\n"),
        )
        .unwrap();
    }
    root
}

/// Empty Trash deletes exactly what was counted, each with its record: a link inside goes and
/// what it points to stays; an item replaced since the count is left alone and named; one put
/// in the Trash after the count stays; a record with no item of its own stays.
#[test]
fn empty_trash_deletes_what_was_counted_and_their_records() {
    use crate::trash::contents;
    let d = tmp("empty");
    let root = fixture_trash(&d, &[("a.txt", "a.txt"), ("dir/", "dir"), ("swapped", "swapped")]);
    fs::create_dir_all(d.join("keep")).unwrap();
    fs::write(d.join("keep/precious"), "keep").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(d.join("keep"), root.join("files/dir/to-keep")).unwrap();
    fs::write(root.join("info/orphan.trashinfo"), "[Trash Info]\nPath=orphan\n").unwrap();
    let bins = [trash_bin(&root)];
    let (items, problem) = contents::list(&bins, &|| false).unwrap();
    assert_eq!((items.len(), problem), (3, None));
    // After the count: one replaced (by a folder, which no platform can take for the file:
    // Windows' identity is creation time, which a new file at the name may inherit), one new.
    // (Made before the file goes, so it can't reuse its inode.)
    fs::create_dir(d.join("folder")).unwrap();
    fs::write(d.join("folder/another"), "another").unwrap();
    fs::remove_file(root.join("files/swapped")).unwrap();
    fs::rename(d.join("folder"), root.join("files/swapped")).unwrap();
    fs::write(root.join("files/late"), "late").unwrap();
    let o = run_now(Op::EmptyTrash { items: Some(items) });
    assert_eq!(o.removed.len(), 2, "{:?}", o.errors);
    assert!(!root.join("files/a.txt").exists() && !root.join("files/dir").exists());
    assert!(!root.join("info/a.txt.trashinfo").exists() && !root.join("info/dir.trashinfo").exists());
    assert_eq!(fs::read_to_string(d.join("keep/precious")).unwrap(), "keep", "a link is removed, never followed");
    assert_eq!(fs::read_to_string(root.join("files/swapped/another")).unwrap(), "another");
    assert!(root.join("info/swapped.trashinfo").exists(), "its record stays with it");
    assert!(o.errors[0].contains("“swapped” isn't the item you chose"), "{:?}", o.errors);
    assert!(root.join("files/late").exists() && root.join("info/orphan.trashinfo").exists());
    assert!(o.undo.is_none(), "it can't be undone");
    assert!(o.folders.iter().any(|(p, _)| *p == root.join("files")), "panes on the trash read it again");
    let _ = fs::remove_dir_all(&d);
}

/// The review's case: an item whose identity couldn't be read when it was counted (its folder
/// unreadable then) was deleted anyway, so a replacement put there since went. Now an item with
/// nothing to check it against is never deleted, and that's said; deleting from a listing too.
#[test]
fn an_item_without_an_identity_is_never_deleted() {
    use crate::trash::contents;
    let d = tmp("no-ident");
    let root = fixture_trash(&d, &[("a", "a")]);
    let (mut items, _) = contents::list(&[trash_bin(&root)], &|| false).unwrap();
    items[0].id = None;
    let o = run_now(Op::EmptyTrash { items: Some(items) });
    assert!(o.removed.is_empty(), "{:?}", o.removed);
    assert!(root.join("files/a").exists() && root.join("info/a.trashinfo").exists());
    assert!(o.errors[0].contains("“a” couldn't be identified"), "{:?}", o.errors);
    fs::write(d.join("b"), "b").unwrap();
    let o = run_now(Op::Delete { items: vec![Doomed { path: d.join("b"), id: None, dir: false }] });
    assert!(o.removed.is_empty() && d.join("b").exists(), "{:?}", o.errors);
    let _ = fs::remove_dir_all(&d);
}

/// Another item at `p` with `p`'s device and inode, as a file system reusing a freed inode at
/// once gives it (ext4 does, every time): `p` rewritten in place, another size. Returns the
/// identity `p` had before, which device and inode alone can't tell from the new one.
#[cfg(unix)]
fn inode_reused(p: &Path) -> crate::fs::Ident {
    let before = crate::fs::ident_at(p).unwrap();
    fs::write(p, "another item, of another size").unwrap();
    assert_eq!(crate::fs::ident_at(p).unwrap().node(), before.node(), "same device and inode");
    before
}

/// The review's blocker: identity was device and inode, so an item replaced by one that got its
/// inode back passed every check and was acted on. Now each destructive path leaves it alone:
/// delete, trash, move, rename, Put Back (search results' identities, `Op::Checked`) and Empty
/// Trash. And a real remove-and-recreate at the same name never inherits the identity.
#[cfg(unix)]
#[test]
fn an_item_with_a_reused_inode_is_never_acted_on() {
    use crate::trash::contents;
    let d = tmp("inode-reuse");
    let target = d.join("t");
    fs::create_dir(d.join("into")).unwrap();
    let checked = |op: Op, id: crate::fs::Ident| Op::Checked { expect: vec![(target.clone(), id)], op: Box::new(op) };
    let ops: Vec<(&str, Box<dyn Fn(crate::fs::Ident) -> Op>)> = vec![
        ("delete", Box::new(|id| Op::Delete { items: vec![Doomed { path: target.clone(), id: Some(id), dir: false }] })),
        ("trash", Box::new(|id| checked(Op::Trash { sources: vec![target.clone()] }, id))),
        ("move", Box::new(|id| checked(Op::Move { sources: vec![target.clone()], dest: d.join("into") }, id))),
        ("rename", Box::new(|id| checked(Op::RenameOne { from: target.clone(), to: d.join("renamed") }, id))),
    ];
    for (what, op) in &ops {
        fs::write(&target, "the item chosen").unwrap();
        let id = inode_reused(&target);
        let o = run_now(op(id));
        assert_eq!(fs::read_to_string(&target).unwrap(), "another item, of another size", "{what}: left alone");
        assert!(o.removed.is_empty() && o.moved.is_empty() && o.results.is_empty(), "{what}");
        assert!(o.refused.is_some() || !o.errors.is_empty(), "{what}: and said");
    }
    // Empty Trash and Put Back.
    let root = fixture_trash(&d, &[("a", "a")]);
    let (items, _) = contents::list(&[trash_bin(&root)], &|| false).unwrap();
    inode_reused(&root.join("files/a"));
    let o = run_now(Op::EmptyTrash { items: Some(items) });
    assert!(o.removed.is_empty() && root.join("files/a").exists() && root.join("info/a.trashinfo").exists(), "empty trash");
    let item = root.join("files/a");
    let id = inode_reused(&item);
    let o = run_now(Op::Checked { expect: vec![(item.clone(), id)], op: Box::new(Op::PutBack { items: vec![item.clone()] }) });
    assert!(o.refused.is_some() && item.exists() && !d.join("a").exists(), "put back");
    // The real thing: removed and made again at once, the same size, twenty times.
    for _ in 0..20 {
        fs::write(&target, "old").unwrap();
        let id = crate::fs::ident_at(&target).unwrap();
        fs::remove_file(&target).unwrap();
        fs::write(&target, "new").unwrap();
        let o = run_now(Op::Delete { items: vec![Doomed { path: target.clone(), id: Some(id), dir: false }] });
        assert!(o.removed.is_empty() && fs::read_to_string(&target).unwrap() == "new", "a replacement is never deleted");
    }
    let _ = fs::remove_dir_all(&d);
}

/// The review's blocker: Empty Trash reached its folders by path, so a trash whose `files/` or
/// `info/` was a link, or a counted `files/` moved away and a link put in its place, had it
/// delete things outside the Trash. Now each folder is opened without following a link and
/// checked against the one counted, and items and records are removed relative to them.
#[cfg(unix)]
#[test]
fn empty_trash_never_deletes_outside_the_trash() {
    use crate::trash::contents;
    use std::os::unix::fs::symlink;
    let d = tmp("empty-outside");
    let outside = d.join("outside");
    let precious = |what: &str| {
        for f in ["a", "a.trashinfo", "precious"] {
            assert!(outside.join(f).exists(), "{what}: outside/{f} was deleted");
        }
    };
    let reset = || {
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&outside).unwrap();
        for f in ["a", "a.trashinfo", "precious"] {
            fs::write(outside.join(f), "not trash").unwrap();
        }
    };
    // The home trash's files/ is a link: it isn't read, and nothing is deleted.
    reset();
    let home = d.join("home/Trash");
    fs::create_dir_all(&home).unwrap();
    symlink(&outside, home.join("files")).unwrap();
    let (items, problem) = contents::list(&[trash_bin(&home)], &|| false).unwrap();
    assert!(items.is_empty() && problem.is_some_and(|p| p.say().contains("isn't a folder of yours")), "{items:?}");
    // A disk's trash whose info/ is a link: its records aren't touched.
    reset();
    let root = d.join("disk/.Trash-x");
    fs::create_dir_all(root.join("files")).unwrap();
    fs::write(root.join("files/a"), "trashed").unwrap();
    symlink(&outside, root.join("info")).unwrap();
    let (items, problem) = contents::list(&[trash_bin(&root)], &|| false).unwrap();
    assert!(items.is_empty() && problem.is_some(), "a trash with a linked info/ isn't emptied: {items:?}");
    precious("info link");
    // Counted, then files/ moved away and a link to it put in its place.
    reset();
    fs::remove_file(root.join("info")).unwrap();
    fs::create_dir(root.join("info")).unwrap();
    fs::write(root.join("info/a.trashinfo"), "[Trash Info]\nPath=a\n").unwrap();
    let (items, _) = contents::list(&[trash_bin(&root)], &|| false).unwrap();
    assert_eq!(items.len(), 1);
    fs::rename(root.join("files"), d.join("moved")).unwrap();
    fs::remove_file(outside.join("a")).unwrap();
    fs::rename(d.join("moved/a"), outside.join("a")).unwrap();
    symlink(&outside, root.join("files")).unwrap();
    let o = run_now(Op::EmptyTrash { items: Some(items) });
    assert!(o.removed.is_empty(), "{:?}", o.removed);
    assert!(o.errors[0].starts_with("Nothing in “") && o.errors[0].contains("isn't a folder of yours"), "{:?}", o.errors);
    precious("files moved and linked");
    assert!(root.join("info/a.trashinfo").exists());
    // And a real folder put in its place (no link) is refused the same way.
    fs::remove_file(root.join("files")).unwrap();
    fs::create_dir(root.join("files")).unwrap();
    fs::write(root.join("files/a"), "trashed").unwrap();
    let (items, _) = contents::list(&[trash_bin(&root)], &|| false).unwrap();
    fs::rename(root.join("files"), d.join("moved2")).unwrap();
    fs::create_dir(root.join("files")).unwrap();
    fs::write(root.join("files/a"), "trashed").unwrap();
    let o = run_now(Op::EmptyTrash { items: Some(items) });
    assert!(o.removed.is_empty() && root.join("files/a").exists(), "{:?}", o.errors);
    assert!(o.errors[0].contains("was moved or replaced since it was counted"), "{:?}", o.errors);
    let _ = fs::remove_dir_all(&d);
}

/// Esc stops it between entries: what's gone stays gone, the rest stays in the Trash.
#[test]
fn empty_trash_stops_when_cancelled() {
    use crate::trash::contents;
    let d = tmp("empty-cancel");
    let root = fixture_trash(&d, &[("a", "a"), ("b", "b"), ("c", "c")]);
    let (mut items, _) = contents::list(&[trash_bin(&root)], &|| false).unwrap();
    items.sort_by(|a, b| a.path.cmp(&b.path));
    let stop = root.join("files/b");
    let o = run_hooked(
        Op::EmptyTrash { items: Some(items) },
        fake_trash,
        Hooks { cancel_at: Some(Box::new(move |p| p == stop)), ..Hooks::default() },
    );
    assert!(o.cancelled);
    assert_eq!(o.removed, [root.join("files/a")]);
    assert!(root.join("files/b").exists() && root.join("info/b.trashinfo").exists() && root.join("files/c").exists());
    let _ = fs::remove_dir_all(&d);
}

/// Put Back reads each item's record: back where it came from (a relative record from the
/// disk's top folder), or beside it under a free name when that's taken (never over it), which
/// the result says; one whose folder is gone stays in the Trash, said so. Records go with the
/// items restored. Undo puts them in the Trash again.
#[test]
fn put_back_restores_from_records_and_never_clobbers() {
    let d = tmp("putback");
    fs::create_dir_all(d.join("docs")).unwrap();
    fs::write(d.join("docs/b.txt"), "someone else's").unwrap();
    let abs = d.join("docs/b.txt").display().to_string().replace(' ', "%20");
    let root = fixture_trash(&d, &[("a.txt", "docs/a.txt"), ("b.txt", &abs), ("c.txt", "gone/c.txt"), ("dir/", "docs/dir")]);
    let files = root.join("files");
    let items = vec![files.join("a.txt"), files.join("b.txt"), files.join("c.txt"), files.join("dir")];
    let o = run_now(Op::PutBack { items });
    assert_eq!(fs::read_to_string(d.join("docs/a.txt")).unwrap(), "a.txt");
    assert_eq!(fs::read_to_string(d.join("docs/b.txt")).unwrap(), "someone else's", "never over what's there");
    assert_eq!(fs::read_to_string(d.join("docs/b 2.txt")).unwrap(), "b.txt");
    assert!(d.join("docs/dir/inner/f").exists());
    assert_eq!(o.notes, ["“b.txt” went back as “b 2.txt”: “b.txt” is taken in “".to_string() + &crate::fs::pretty(&d.join("docs")) + "”"]);
    assert_eq!(o.errors.len(), 1, "{:?}", o.errors);
    assert!(o.errors[0].starts_with("“c.txt” stayed in the Trash: the folder it came from"), "{:?}", o.errors);
    assert!(files.join("c.txt").exists() && root.join("info/c.txt.trashinfo").exists());
    for n in ["a.txt", "b.txt", "dir"] {
        assert!(!files.join(n).exists() && !root.join(format!("info/{n}.trashinfo")).exists(), "{n}");
    }
    assert!(o.folders.iter().any(|(p, _)| *p == d.join("docs")), "the pane on its folder reads it again");
    let u = o.undo.expect("undoable");
    assert_eq!(u.what, What::PutBack);
    let back = run_now(Op::Undo(u));
    assert!(back.errors.is_empty() && back.refused.is_none(), "{:?} {:?}", back.errors, back.refused);
    assert!(!d.join("docs/a.txt").exists() && !d.join("docs/b 2.txt").exists() && d.join("docs/b.txt").exists());
    assert!(d.join("docs/.trash/a.txt").exists(), "undo trashed it again");
    let _ = fs::remove_dir_all(&d);
}

/// An item replaced in the Trash since it was listed is left alone (its record too).
#[test]
fn put_back_checks_each_item_is_still_the_one_listed() {
    let d = tmp("putback-swap");
    fs::create_dir_all(d.join("docs")).unwrap();
    let root = fixture_trash(&d, &[("a.txt", "docs/a.txt")]);
    let item = root.join("files/a.txt");
    let id = crate::fs::ident_at(&item).unwrap();
    // Replaced by a folder (see `empty_trash_deletes_what_was_counted_and_their_records`).
    fs::create_dir(d.join("folder")).unwrap();
    fs::remove_file(&item).unwrap();
    fs::rename(d.join("folder"), &item).unwrap();
    let o = run_now(Op::Checked { expect: vec![(item.clone(), id)], op: Box::new(Op::PutBack { items: vec![item.clone()] }) });
    assert!(o.refused.is_some_and(|r| r.contains("“a.txt”")));
    assert!(item.exists() && !d.join("docs/a.txt").exists());
    let _ = fs::remove_dir_all(&d);
}
