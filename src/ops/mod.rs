//! File operations: copy, move, trash, permanent delete, and undo. Each runs on its own worker thread and reports
//! progress through atomics; the UI reads them in `poll`.
//!
//! Safety rules: nothing is ever overwritten. Every move, rename and copy uses the no-replace
//! primitives in [`crate::noclobber`], so a name taken between choosing it and using it fails
//! instead of destroying what took it; operations then pick another free name ("name copy" in
//! the same folder, "name 2" elsewhere), and undo leaves that item where it is. Folders can't go
//! into themselves. Undo refuses rather than guesses: every result is fingerprinted when the
//! operation finishes, undo checks all of them before touching anything, and checks each again
//! right before its own step (see `print.rs`).

use std::fs::Metadata;
use std::hash::{Hash, Hasher};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::noclobber;
use crate::trash::TrashError;

mod batch;
mod links;
mod print;
#[cfg(test)]
mod tests;
mod trashed;
mod undo;
mod worker;

use links::*;
pub use links::{MAKING_LINKS, NEW_FILE};
pub use print::Seen;
use print::*;
pub use undo::Undo;
use undo::*;
use worker::*;

/// How many free names an operation tries for one item when each is taken before it can be
/// used (another app creating files there at the same moment).
const TRIES: usize = 32;

pub enum Op {
    Copy {
        sources: Vec<PathBuf>,
        dest: PathBuf,
    },
    Move {
        sources: Vec<PathBuf>,
        dest: PathBuf,
    },
    Trash {
        sources: Vec<PathBuf>,
    },
    /// Delete permanently, confirmed first in Arbor's one dialog: each item (a folder with
    /// everything in it, links not followed) only while it's still the item the user chose,
    /// checked right before it goes ([`Doomed::id`]).
    Delete {
        items: Vec<Doomed>,
    },
    /// Batch rename: every item gets its new name in its own folder, or none does (see
    /// [`Worker::rename_all`]).
    Rename {
        items: Vec<Renaming>,
    },
    /// Empty the Trash, confirmed first in Arbor's one dialog: the items its count found
    /// (each only while it's still that item), or when the count hadn't landed, what's there
    /// when the worker starts (`trash::contents`). Windows empties the shell's Recycle Bin.
    EmptyTrash {
        items: Option<Vec<crate::trash::contents::Item>>,
    },
    /// Put items in a freedesktop.org trash's `files/` back where their `.trashinfo` says they
    /// came from, under a free name when that's taken (see [`Worker::put_back`]).
    PutBack {
        items: Vec<PathBuf>,
    },
    /// The inline rename: `from` becomes `to` in its folder, never over another item.
    RenameOne {
        from: PathBuf,
        to: PathBuf,
    },
    /// New Folder: "untitled folder" (or a free variant) in `dir`.
    NewFolder {
        dir: PathBuf,
    },
    /// New File: an empty "untitled.txt" (or a free variant) in `dir`, made with `create_new`.
    NewFile {
        dir: PathBuf,
    },
    /// Make Symbolic Link: for each item, a link beside it ("name link", free), pointing at the
    /// item's absolute path; on Windows without the right to make one, a shortcut (`.lnk`).
    MakeLinks {
        items: Vec<PathBuf>,
    },
    /// Reverse an earlier operation, if the disk still looks the way it left it.
    Undo(Undo),
    /// `op` on items chosen from a listing or search results: each item in `expect` must still
    /// be the object the listing or the walk found at its path (its [`crate::fs::Ident`],
    /// edited or not); one that isn't (moved or deleted since, and maybe something else put
    /// there) is left out and said so, and the rest go ahead.
    Checked {
        expect: Checks,
        op: Box<Op>,
    },
}

/// Where the items an operation was asked for were chosen, for what a refusal says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Chosen {
    /// A folder's listing: reading it again shows what's there now.
    #[default]
    Listing,
    /// Search results.
    Search,
}

/// The items an operation was asked for, each with the identity it had when its listing or
/// search walk read it ([`Op::Checked`]); `None` when it couldn't be read then, and so can't be
/// told from a replacement: such an item is never moved or removed (copying it is harmless).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Checks {
    pub from: Chosen,
    pub items: Vec<(PathBuf, Option<crate::fs::Ident>)>,
}

impl Checks {
    pub fn new(from: Chosen, items: Vec<(PathBuf, Option<crate::fs::Ident>)>) -> Checks {
        Checks { from, items }
    }

    /// From search results, every identity known (tests).
    #[cfg(test)]
    pub fn search(items: Vec<(PathBuf, crate::fs::Ident)>) -> Checks {
        Checks { from: Chosen::Search, items: items.into_iter().map(|(p, id)| (p, Some(id))).collect() }
    }
}

/// Making a new folder, as the status bar says it.
pub const NEW_FOLDER: &str = "Making a New Folder";

/// Emptying the Trash, as the status bar says it.
pub const EMPTYING: &str = if cfg!(windows) { "Emptying the Recycle Bin" } else { "Emptying the Trash" };

impl Op {
    /// Whether Esc can stop it: everything but the Recycle Bin's emptying, which the shell does
    /// in one call.
    fn cancellable(&self) -> bool {
        !(crate::trash::contents::SHELL && matches!(self, Op::EmptyTrash { .. }))
    }

    /// The folders it changes: where items go and where they come from.
    fn folders(&self) -> Vec<PathBuf> {
        let parents = |ps: &[PathBuf]| ps.iter().filter_map(|p| p.parent().map(Path::to_path_buf)).collect::<Vec<_>>();
        let mut v = match self {
            Op::Copy { dest, .. } => vec![dest.clone()],
            Op::Move { sources, dest } => [vec![dest.clone()], parents(sources)].concat(),
            Op::Trash { sources } | Op::PutBack { items: sources } => parents(sources),
            // Which trash folders it empties is known on the worker (`Worker::empty_trash`).
            Op::EmptyTrash { .. } => Vec::new(),
            Op::Delete { items } => items.iter().filter_map(|d| d.path.parent().map(Path::to_path_buf)).collect(),
            Op::Rename { items } => items.iter().filter_map(|r| r.from.parent().map(Path::to_path_buf)).collect(),
            Op::RenameOne { from, .. } => from.parent().map(Path::to_path_buf).into_iter().collect(),
            Op::NewFolder { dir } | Op::NewFile { dir } => vec![dir.clone()],
            Op::MakeLinks { items } => parents(items),
            Op::Checked { op, .. } => return op.folders(),
            Op::Undo(u) => u
                .steps
                .iter()
                .flat_map(|s| [Some(&s.at), s.back.as_ref()])
                .flatten()
                .filter_map(|p| p.parent().map(Path::to_path_buf))
                .collect(),
        };
        v.sort();
        v.dedup();
        v
    }

    pub fn verb(&self) -> &'static str {
        match self {
            Op::Copy { .. } => "Copying",
            Op::Move { .. } => "Moving",
            Op::Trash { .. } => "Moving to Trash",
            Op::Delete { .. } => "Deleting",
            Op::EmptyTrash { .. } => EMPTYING,
            Op::PutBack { .. } => "Putting Back",
            Op::RenameOne { .. } => "Renaming",
            Op::NewFolder { .. } => NEW_FOLDER,
            Op::NewFile { .. } => NEW_FILE,
            Op::MakeLinks { .. } => MAKING_LINKS,
            Op::Rename { .. } => "Renaming",
            Op::Undo(u) if u.redo => "Redoing",
            Op::Undo(_) => "Undoing",
            Op::Checked { op, .. } => op.verb(),
        }
    }

    /// The operation without the items at `left_out`, and whether any are left.
    fn without(self, left_out: &std::collections::HashSet<&Path>) -> (Op, bool) {
        let keep = |v: &mut Vec<PathBuf>| v.retain(|p| !left_out.contains(p.as_path()));
        match self {
            Op::Copy { mut sources, dest } => {
                keep(&mut sources);
                let any = !sources.is_empty();
                (Op::Copy { sources, dest }, any)
            }
            Op::Move { mut sources, dest } => {
                keep(&mut sources);
                let any = !sources.is_empty();
                (Op::Move { sources, dest }, any)
            }
            Op::Trash { mut sources } => {
                keep(&mut sources);
                let any = !sources.is_empty();
                (Op::Trash { sources }, any)
            }
            Op::RenameOne { from, to } => {
                let any = !left_out.contains(from.as_path());
                (Op::RenameOne { from, to }, any)
            }
            Op::PutBack { mut items } => {
                keep(&mut items);
                let any = !items.is_empty();
                (Op::PutBack { items }, any)
            }
            Op::MakeLinks { mut items } => {
                keep(&mut items);
                let any = !items.is_empty();
                (Op::MakeLinks { items }, any)
            }
            Op::Delete { mut items } => {
                items.retain(|d| !left_out.contains(d.path.as_path()));
                let any = !items.is_empty();
                (Op::Delete { items }, any)
            }
            Op::Rename { mut items } => {
                items.retain(|r| !left_out.contains(r.from.as_path()));
                let any = !items.is_empty();
                (Op::Rename { items }, any)
            }
            op => (op, true),
        }
    }
}

/// An item to delete permanently: where it is, what it was when the user chose it (its
/// [`crate::fs::ident`], `None` when that couldn't be read), and whether it's a folder (the
/// dialog says "with everything in it").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doomed {
    pub path: PathBuf,
    pub id: Option<crate::fs::Ident>,
    pub dir: bool,
}

impl Doomed {
    /// `path` as it is now (one lstat).
    pub fn now(path: &Path) -> Doomed {
        let m = path.symlink_metadata().ok();
        Doomed { path: path.to_path_buf(), id: m.as_ref().and_then(|m| crate::fs::ident_of(path, m)), dir: m.is_some_and(|m| m.is_dir()) }
    }
}

/// One item of a batch rename: where it is, its new name's path (in the same folder), and what
/// it looked like when the preview was made.
#[derive(Debug, Clone)]
pub struct Renaming {
    pub from: PathBuf,
    pub to: PathBuf,
    pub seen: Seen,
}

/// What an undo reverses, for its messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum What {
    Rename,
    /// A batch rename: undone as one, through the same two-step plan.
    BatchRename,
    Move,
    Copy,
    Duplicate,
    NewFolder,
    NewFile,
    /// Links Arbor made: undone by removing each (only while it's still the link made, with
    /// the same target), redone by making it again ([`Worker::undo_links`]).
    Link,
    Trash,
    /// Put Back from the Trash: undone by trashing the items again.
    PutBack,
}

impl What {
    fn noun(self) -> &'static str {
        match self {
            What::Rename => "rename",
            What::BatchRename => "batch rename",
            What::Move => "move",
            What::Copy => "copy",
            What::Duplicate => "duplicate",
            What::NewFolder => "new folder",
            What::NewFile => "new file",
            What::Link => "link",
            What::Trash => "move to Trash",
            What::PutBack => "put back",
        }
    }
}

#[derive(Default)]
pub struct Progress {
    pub bytes_total: AtomicU64,
    pub bytes_done: AtomicU64,
    pub items_total: AtomicUsize,
    pub items_done: AtomicUsize,
    pub cancel: AtomicBool,
}

pub struct Outcome {
    /// Where each item ended up (for selecting them).
    pub results: Vec<PathBuf>,
    /// For an operation, how to reverse it. For an undo, what's left to undo: all of it when it
    /// was refused, failed or cancelled before changing anything, or the items that didn't go
    /// back. The UI puts it back on the stack, so a failed undo can be tried again and the items
    /// that did go back are never undone twice.
    pub undo: Option<Undo>,
    /// For an undo or a redo, the steps that went, pointing back the other way: what the UI
    /// puts on the redo stack after an undo, and on the undo stack after a redo.
    pub inverse: Option<Undo>,
    pub errors: Vec<String>,
    /// An undo that changed nothing because the disk changed since the operation: why.
    pub refused: Option<String>,
    pub cancelled: bool,
    /// Items that moved or got a new name: `(from, to)`, so panes and searches can show them
    /// where they are now at once.
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// Items that went to the Trash, or were deleted (or are gone from where they were).
    pub removed: Vec<PathBuf>,
    /// Items a trash left where they were because their disk has no trash that takes them
    /// ([`TrashError::NoTrash`]), each as it was then: the UI asks whether to delete them
    /// permanently. `no_trash_why` is the first one's reason.
    pub no_trash: Vec<Doomed>,
    pub no_trash_why: Option<String>,
    /// A permanent delete stopped (cancelled or failed) inside this folder: part of it is gone.
    pub partly: Option<PathBuf>,
    /// What the operation says instead of the usual message (emptying the Recycle Bin, or a
    /// Trash Arbor isn't allowed to read).
    pub said: Option<String>,
    /// Said after the usual message: an item Put Back gave a free name, say.
    pub notes: Vec<String>,
    /// Nothing to say when it goes well (the inline rename, New Folder: what changed is in
    /// the list).
    pub quiet: bool,
    /// Each of `folders` and its canonical path (links resolved), so a search through a
    /// symlink to one of them knows them by its own paths.
    pub canon: Vec<(PathBuf, PathBuf)>,
    /// The folders the operation may have changed, with their identities (links followed), so
    /// the panes showing them, by any path, read them again.
    pub folders: Vec<(PathBuf, Option<noclobber::DirId>)>,
    /// Where identity is told by ctime alone ([`crate::fs::Ident::by_ctime`]): each identity an
    /// item had when it was chosen, and the one Arbor's own action gave it, read right after
    /// (renaming, moving or trashing an item changes its ctime, and its other names' too). Rows
    /// still showing the first take the second ([`reidentified`]), so the next action on them,
    /// before their folder is read again, doesn't take Arbor's change for a replacement.
    pub reidentified: Vec<(crate::fs::Ident, crate::fs::Ident)>,
    /// New File and New Folder: what they made, as an identity (read right after), so the
    /// rename starts on that item and no other row.
    pub made: Option<crate::fs::Ident>,
}

/// `id` as Arbor's own action left it, from an outcome's [`Outcome::reidentified`]; `id` when
/// it wasn't touched.
pub fn reidentified(id: crate::fs::Ident, pairs: &[(crate::fs::Ident, crate::fs::Ident)]) -> crate::fs::Ident {
    pairs.iter().find(|(was, _)| *was == id).map_or(id, |&(_, now)| now)
}

pub struct Running {
    pub verb: &'static str,
    /// Esc can stop it (see [`Op::cancellable`]).
    pub cancellable: bool,
    pub progress: Arc<Progress>,
    done: Arc<Mutex<Option<Outcome>>>,
}

impl Running {
    pub fn take(&self) -> Option<Outcome> {
        self.done.lock().ok()?.take()
    }
    pub fn cancel(&self) {
        self.progress.cancel.store(true, Relaxed);
    }
    /// Tests: whether the outcome is in, without taking it.
    #[cfg(test)]
    pub fn finished(&self) -> bool {
        self.done.lock().is_ok_and(|d| d.is_some())
    }
}

/// The trash: moves one item, checking the second argument ("stop?") during any long work
/// before it.
type TrashFn = fn(&Path, &dyn Fn() -> bool) -> Result<Option<PathBuf>, TrashError>;

/// The trash [`start`] uses: the platform's, or in tests a `.trash` folder beside each item, so
/// no test fills the user's own Trash.
#[cfg(not(test))]
const TRASH: TrashFn = crate::trash::trash;
#[cfg(test)]
const TRASH: TrashFn = tests::fake_trash;

/// How the folder a copy or move goes into was known when the user chose it: what the worker
/// compares with what's at its path before touching anything ([`start`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DestId {
    /// The folder's own identity, links followed (`Folder::dir_id`, a column's): a paste, a drop
    /// on a list's or a column's background, on the other pane.
    Folder(noclobber::DirId),
    /// A folder row's identity as its listing read it (not following a link): a drop on a row.
    Item(crate::fs::Ident),
}

impl DestId {
    /// Whether the folder at `path` is still the one it was.
    pub fn holds(self, path: &Path) -> bool {
        match self {
            DestId::Folder(id) => noclobber::dir_id(path) == Some(id),
            DestId::Item(id) => crate::fs::ident_at(path) == Some(id),
        }
    }
}

/// Where a drop goes: a folder on screen, and how its listing knew it (`None`: it didn't, as a
/// folder whose read hasn't landed).
#[derive(Clone, Debug, PartialEq)]
pub struct Dest {
    pub path: PathBuf,
    pub id: Option<DestId>,
}

/// Runs `op` on a worker thread; its outcome comes back through [`Running::take`].
///
/// With `dest_id`, a copy, move, new file or new folder goes only into the folder the user chose
/// as the one with that identity (a paste, asked for before the clipboard answered; a drop on a
/// column; New File in the folder as it was listed): if the
/// folder at its `dest` isn't that one when the worker starts (renamed away and another made
/// at its path, or gone), it refuses before touching anything, saying "Nothing was" and the
/// verb given with it ("pasted", "dropped").
pub fn start(op: Op, dest_id: Option<(DestId, &'static str)>, waker: Option<rune::Waker>) -> Running {
    let progress = Arc::new(Progress::default());
    let done = Arc::new(Mutex::new(None));
    let verb = op.verb();
    let cancellable = op.cancellable();
    let (p, d) = (progress.clone(), done.clone());
    // Headless runs (scripts, snapshots) run on a virtual clock far faster than real time, so
    // they do the work inline to stay deterministic.
    if rune::tooling::headless_run().is_some() {
        let outcome = Worker::new(&p, None, TRASH).run_into(op, dest_id);
        *done.lock().expect("fresh mutex") = Some(outcome);
        return Running { verb, cancellable, progress, done };
    }
    std::thread::spawn(move || {
        let outcome = Worker::new(&p, waker.as_ref(), TRASH).run_into(op, dest_id);
        if let Ok(mut slot) = d.lock() {
            *slot = Some(outcome);
        }
        if let Some(wake) = &waker {
            wake();
        }
    });
    Running { verb, cancellable, progress, done }
}

/// Renames `from` to `to` in place (the inline rename), never over another item. A case-only
/// change on a case-insensitive disk names the item itself and goes through. The error is the
/// whole status-bar message.
pub fn rename(from: &Path, to: &Path) -> Result<(), String> {
    rename_between(from, to, || {})
}

/// [`rename`], running `between` after the up-front check (tests put an item at `to` there).
fn rename_between(from: &Path, to: &Path, between: impl FnOnce()) -> Result<(), String> {
    let taken = || format!("“{}” already exists", name_of(to));
    if to.symlink_metadata().is_ok() && !noclobber::same_item(from, to) {
        return Err(taken());
    }
    between();
    match noclobber::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Err(taken()),
        Err(e) => Err(format!("Couldn't rename: {e}")),
    }
}

/// Makes a new empty folder named `name` (or a free variant) in `dir`; returns its path.
pub fn new_folder(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let mut last = None;
    for _ in 0..TRIES {
        let path = free_name(dir, std::ffi::OsStr::new(name), false);
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            // Taken since `free_name` looked: try the next free name.
            Err(e) if e.kind() == ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::from(ErrorKind::AlreadyExists)))
}

/// Runs an operation on this thread, trashing with `trash` (tests).
#[cfg(test)]
fn run_with(op: Op, trash: TrashFn) -> Outcome {
    run_hooked(op, trash, Hooks::default())
}

#[cfg(test)]
fn run_hooked(op: Op, trash: TrashFn, hooks: Hooks) -> Outcome {
    let p = Progress::default();
    let mut w = Worker::new(&p, None, trash);
    w.hooks = hooks;
    w.run(op)
}

#[cfg(test)]
type PathHook = Box<dyn FnMut(&Path)>;
#[cfg(test)]
type PathTest = Box<dyn Fn(&Path) -> bool>;
#[cfg(test)]
type RemovalHook = Box<dyn FnMut(links::Removal, &Path)>;

/// Test-only ways to make the worker meet the disk at its worst. Not compiled outside tests.
#[cfg(test)]
#[derive(Default)]
struct Hooks {
    /// Called with the destination right before each destructive step, after every check:
    /// where a test makes another app's file appear.
    before_act: Option<PathHook>,
    /// Treat every rename as crossing disks, so moves copy then delete.
    cross_device: bool,
    /// Fail renaming onto these paths (a batch rename failing midway).
    fail_rename: Option<PathTest>,
    /// Fail removing these items (a permission error halfway through a delete).
    fail_remove: Option<PathTest>,
    /// Cancel when a permanent delete is about to remove one of these (the user pressing Esc
    /// midway).
    cancel_at: Option<PathTest>,
    /// Called with each file or folder a copy makes, right after it's made: where a test
    /// replaces it or cancels.
    after_made: Option<PathHook>,
    /// Called with each file a copy wrote, before anything else looks at it: where a test
    /// replaces it as if another app did so while a stalled copy was still writing.
    copied: Option<PathHook>,
    /// The mount table, instead of the system's (a folder a test says is a mount point).
    mounts: Option<crate::mounts::Mounts>,
    /// Called at each step of a link's removal (`links::Removal`), with the link's path: where a
    /// test swaps the link at the instant of removal.
    removing: Option<RemovalHook>,
}

/// A path for comparing names case aside (what a disk that ignores case sees as one name).
fn fold_path(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

/// Whether `e` means the item isn't there: not found, or a folder on its path is now a file.
/// Anything else (a permission error, say) means it may still be there.
fn is_gone(e: &io::Error) -> bool {
    matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
}

/// Removes what a failed or cancelled copy made, deepest first, and only what it made: each file
/// or link only while its fingerprint (identity, size, modified time) still matches the one taken
/// right after the copy made it, each folder only while it's the same folder (identity) and
/// empty. Whatever another app replaced, edited or put inside meanwhile stays, and so do the
/// folders holding it. Returns what was left and why, for the status bar.
///
/// Removed permanently, not to the Trash: these are Arbor's own fresh output from an operation
/// that didn't finish, never handed to the user (no result, no undo record), and each is
/// re-checked against its fingerprint right before it goes. Removing them puts the disk back
/// as it was before the copy, which is what cancel means; the sources are untouched. Trashing
/// them would fill the Trash with half-copies, and on a disk with no trash it would need a
/// confirmation for data that was never the user's. Same limits as undo's checks (see
/// [`Print`]): an edit within the timestamp granularity that keeps the size isn't seen, and
/// there's an instant between the check and the removal.
fn unmake(made: &[Made]) -> Option<String> {
    let mut kept: Vec<(&Path, &str)> = Vec::new();
    for m in made.iter().rev() {
        let now = match m.dst.symlink_metadata() {
            Ok(now) => Print::of(&m.dst, &now),
            Err(_) => continue, // gone already, or unreachable: nothing to remove
        };
        let ours = m
            .made
            .as_ref()
            .is_some_and(|was| if m.dir { now.kind == was.kind && now.id.is_some() && now.id == was.id } else { now == *was });
        if !ours {
            kept.push((&m.dst, "it changed after Arbor made it"));
            continue;
        }
        let gone = if m.dir { std::fs::remove_dir(&m.dst) } else { std::fs::remove_file(&m.dst) };
        // A folder holding something kept below it: that one is already said.
        if gone.as_ref().is_err_and(|e| e.kind() != ErrorKind::NotFound) && !kept.iter().any(|(k, _)| k.starts_with(&m.dst)) {
            kept.push((&m.dst, if m.dir { "something else is inside it" } else { "it couldn't be removed" }));
        }
    }
    let (at, why) = kept.last()?; // the shallowest, which holds or neighbours the rest
    let more = if kept.len() > 1 { format!(" (and {} more)", kept.len() - 1) } else { String::new() };
    Some(format!("kept “{}” in “{}”: {why}{more}", name_of(at), parent_of(at)))
}

/// `why` an operation failed, and what its cleanup had to leave.
fn with_kept(why: String, kept: Option<String>) -> String {
    match kept {
        Some(k) => format!("{why}; {k}"),
        None => why,
    }
}

fn name_of(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

fn parent_of(p: &Path) -> String {
    p.parent().map_or_else(|| p.display().to_string(), |d| d.display().to_string())
}

/// Total bytes of regular files under `p`, not following links.
fn tree_size(p: &Path) -> u64 {
    let Ok(m) = p.symlink_metadata() else { return 0 };
    if m.is_dir() {
        std::fs::read_dir(p).map_or(0, |rd| rd.flatten().map(|de| tree_size(&de.path())).sum())
    } else if m.is_file() {
        m.len()
    } else {
        0
    }
}

#[cfg(unix)]
fn make_symlink(target: &Path, at: &Path, _is_dir: bool) -> io::Result<()> {
    std::os::unix::fs::symlink(target, at)
}

#[cfg(windows)]
fn make_symlink(target: &Path, at: &Path, is_dir: bool) -> io::Result<()> {
    if is_dir { std::os::windows::fs::symlink_dir(target, at) } else { std::os::windows::fs::symlink_file(target, at) }
}

/// A free path for `name` in `dir`: the name itself if nothing's there, else
/// "stem copy.ext", "stem copy 2.ext" (same folder) or "stem 2.ext" (elsewhere).
/// Free when it looked: callers use it with a no-replace primitive and try again if it's taken.
pub fn free_name(dir: &Path, name: &std::ffi::OsStr, same_dir: bool) -> PathBuf {
    let first = dir.join(name);
    if !same_dir && first.symlink_metadata().is_err() {
        return first;
    }
    let s = name.to_string_lossy();
    let is_dir = first.is_dir();
    let (stem, ext) = match s.rfind('.') {
        Some(i) if i > 0 && !is_dir => (&s[..i], &s[i..]),
        _ => (&s[..], ""),
    };
    for n in 1.. {
        let cand = match (same_dir, n) {
            (true, 1) => format!("{stem} copy{ext}"),
            (true, n) => format!("{stem} copy {n}{ext}"),
            (false, n) => format!("{stem} {}{ext}", n + 1),
        };
        let p = dir.join(cand);
        if p.symlink_metadata().is_err() {
            return p;
        }
    }
    unreachable!()
}

/// ", 3 left", or ", some items left (count unknown: couldn't read “x/y”)" when part of what's
/// left couldn't be read (shown from `item`'s folder), or nothing when the count was stopped.
fn left_note(left: Option<crate::delete::Left>, item: &Path) -> String {
    match left {
        None => String::new(),
        Some(crate::delete::Left::Exactly(n)) => format!(", {n} left"),
        Some(crate::delete::Left::Unknown(at)) => {
            let shown = item.parent().and_then(|p| at.strip_prefix(p).ok()).unwrap_or(&at).display().to_string();
            format!(", some items left (count unknown: couldn't read “{shown}”)")
        }
    }
}
