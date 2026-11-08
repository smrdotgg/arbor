//! File operations: copy, move, trash, permanent delete, and undo. Each runs on its own worker thread and reports
//! progress through atomics; the UI reads them in `poll`.
//!
//! Safety rules: nothing is ever overwritten. Every move, rename and copy uses the no-replace
//! primitives in [`crate::noclobber`], so a name taken between choosing it and using it fails
//! instead of destroying what took it; operations then pick another free name ("name copy" in
//! the same folder, "name 2" elsewhere), and undo leaves that item where it is. Folders can't go
//! into themselves. Undo refuses rather than guesses: every result is fingerprinted when the
//! operation finishes, undo checks all of them before touching anything, and checks each again
//! right before its own step (see [`Print`]).

use std::fs::Metadata;
use std::hash::{Hash, Hasher};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::noclobber;
use crate::trash::TrashError;

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
    /// Reverse an earlier operation, if the disk still looks the way it left it.
    Undo(Undo),
    /// `op` on search results: each item in `expect` must still be the one the walk found at
    /// its path (its [`crate::fs::ident`]); one that isn't (moved or deleted since, and maybe
    /// something else put there) is left out and said so, and the rest go ahead.
    Checked {
        expect: Vec<(PathBuf, crate::fs::Ident)>,
        op: Box<Op>,
    },
}

impl Op {
    /// The folders it changes: where items go and where they come from.
    fn folders(&self) -> Vec<PathBuf> {
        let parents = |ps: &[PathBuf]| ps.iter().filter_map(|p| p.parent().map(Path::to_path_buf)).collect::<Vec<_>>();
        let mut v = match self {
            Op::Copy { dest, .. } => vec![dest.clone()],
            Op::Move { sources, dest } => [vec![dest.clone()], parents(sources)].concat(),
            Op::Trash { sources } => parents(sources),
            Op::Delete { items } => items.iter().filter_map(|d| d.path.parent().map(Path::to_path_buf)).collect(),
            Op::Rename { items } => items.iter().filter_map(|r| r.from.parent().map(Path::to_path_buf)).collect(),
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
            Op::Rename { .. } => "Renaming",
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
        Doomed { path: path.to_path_buf(), id: m.as_ref().and_then(crate::fs::ident), dir: m.is_some_and(|m| m.is_dir()) }
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

/// An item's fingerprint, taken when the batch rename's preview read its folder, so applying
/// refuses an item that changed since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen(Print);

impl Seen {
    /// `path` as it is now (not following a link); `None` when it isn't there.
    #[cfg(test)]
    pub fn of(path: &Path) -> Option<Seen> {
        Some(Seen::with(path, &path.symlink_metadata().ok()?))
    }

    /// From metadata just read for `path`.
    pub fn with(path: &Path, m: &std::fs::Metadata) -> Seen {
        Seen(Print::of(path, m))
    }
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
    Trash,
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
            What::Trash => "move to Trash",
        }
    }
}

/// How to reverse a finished operation: each step's item, as it was left, goes back to where it
/// came from or (for what the operation made) to the Trash.
#[derive(Debug, Clone)]
pub struct Undo {
    pub what: What,
    steps: Vec<Step>,
}

#[derive(Debug, Clone)]
struct Step {
    /// Where the operation left the item.
    at: PathBuf,
    /// What it looked like right after.
    print: Print,
    /// Where it goes back to; `None` trashes it (a copy, duplicate or new folder).
    back: Option<PathBuf>,
    /// An earlier try at this step moved it back across disks (copy, then delete) and the
    /// delete didn't finish.
    split: Option<Split>,
}

/// A move back across disks that copied everything to `back` but couldn't remove all of the
/// original from `at`. Both halves are fingerprinted as that try left them, so a retry can
/// finish removing the rest only while neither has changed, and otherwise says where things are.
#[derive(Debug, Clone)]
struct Split {
    /// The complete copy at `back`.
    copy: Print,
    /// What's still at `at`.
    rest: Print,
}

impl Undo {
    /// Fingerprints each `(at, back)` as it is now. An item that can't be read (gone already)
    /// can't be undone: it's left out, and said so in `errors`, never silently.
    fn record_or_say(what: What, items: impl IntoIterator<Item = (PathBuf, Option<PathBuf>)>, errors: &mut Vec<String>) -> Option<Undo> {
        let mut steps = Vec::new();
        for (at, back) in items {
            match at.symlink_metadata() {
                Ok(m) => steps.push(Step { print: Print::of(&at, &m), at, back, split: None }),
                Err(e) => errors.push(format!("“{}” can't be undone: it couldn't be read right after ({e})", name_of(&at))),
            }
        }
        (!steps.is_empty()).then_some(Undo { what, steps })
    }

    /// [`Undo::record_or_say`] for one item done on the window's thread: `None` (with nothing
    /// to undo) when it can't be read.
    fn record(what: What, items: impl IntoIterator<Item = (PathBuf, Option<PathBuf>)>) -> Option<Undo> {
        Undo::record_or_say(what, items, &mut Vec::new())
    }

    /// A rename that just happened (`from` is now `to`).
    pub fn renamed(to: PathBuf, from: PathBuf) -> Option<Undo> {
        Undo::record(What::Rename, [(to, Some(from))])
    }

    /// A folder just made, empty.
    pub fn new_folder(path: PathBuf) -> Option<Undo> {
        let mut u = Undo::record(What::NewFolder, [(path, None)])?;
        u.steps[0].print.tree = Some(Tree::default());
        Some(u)
    }
}

/// A cheap fingerprint of what an operation left on disk, taken when it finished. Undo compares
/// it with the disk and refuses on any difference, so a wrong undo can't happen silently.
///
/// - Identity: device and inode on Unix, volume serial number and file index on Windows
///   ([`noclobber::file_id`]), so a file deleted and recreated under the same name reads as
///   replaced.
/// - Type, size (files and links) and modified time. A folder's own mtime changes when an
///   entry directly inside it is added, removed or renamed.
/// - `tree`: for folders whose undo trashes them (copies, duplicates, new folders), a sum over
///   everything inside. A folder's mtime doesn't see an edit deeper down, and trashing a copy the
///   user has since worked in is exactly the wrong undo this exists to stop. Moves and renames
///   don't walk: moving a folder back carries whatever is inside with it and loses nothing, so a
///   walk would cost O(tree) to protect nothing.
///
/// Known limits (contents are never hashed: far too slow for big files):
///
/// - An edit that keeps the size and leaves the mtime where it was isn't seen. That includes an
///   edit made within the disk's timestamp granularity of the fingerprint: 2 s on FAT, 1 s on
///   HFS+ and ext3, 100 ns on NTFS, 1 ns on APFS and ext4 (but ext4 and NTFS stamp from a clock
///   that ticks every few ms, so two writes in the same tick look alike). Pinned by the test
///   `same_size_edit_within_mtime_granularity_is_not_seen`.
/// - The instant between the last check and the act. Each step is checked right before it runs,
///   and moves can't overwrite whatever appears in that instant.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Print {
    kind: Kind,
    size: u64,
    mtime: Option<SystemTime>,
    id: Option<(u64, u64)>,
    tree: Option<Tree>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    File,
    Folder,
    Link,
}

impl Kind {
    fn of(m: &Metadata) -> Kind {
        let ft = m.file_type();
        if ft.is_symlink() {
            Kind::Link
        } else if ft.is_dir() {
            Kind::Folder
        } else {
            Kind::File
        }
    }
    fn name(self) -> &'static str {
        match self {
            Kind::File => "a file",
            Kind::Folder => "a folder",
            Kind::Link => "a link",
        }
    }
}

impl Print {
    fn of(path: &Path, m: &Metadata) -> Print {
        Print::with_id(m, noclobber::file_id(path, m))
    }

    /// From metadata and an identity read off a handle (`noclobber::Made`), not by name.
    fn with_id(m: &Metadata, id: Option<(u64, u64)>) -> Print {
        let kind = Kind::of(m);
        Print { kind, size: if kind == Kind::Folder { 0 } else { m.len() }, mtime: m.modified().ok(), id, tree: None }
    }
}

/// How an item differs from its fingerprint.
enum Diff {
    Gone,
    Kind(Kind),
    Replaced,
    Edited,
    Inside(std::cmp::Ordering),
    Unreadable,
    /// It can't be looked at (its folder can't be read): not known to be gone.
    Unreachable(io::Error),
}

impl Diff {
    /// "“name” was edited", for an item that was `was`.
    fn say(&self, name: &str, was: Kind) -> String {
        match self {
            Diff::Gone => format!("{name} was moved or deleted"),
            Diff::Kind(k) => format!("{name} was replaced by {}", k.name()),
            Diff::Replaced => format!("{name} was replaced by another item with the same name"),
            Diff::Edited if was == Kind::Folder => format!("items were added, removed or renamed in {name}"),
            Diff::Edited => format!("{name} was edited"),
            Diff::Inside(std::cmp::Ordering::Greater) => format!("items were added inside {name}"),
            Diff::Inside(std::cmp::Ordering::Less) => format!("items were removed from inside {name}"),
            Diff::Inside(std::cmp::Ordering::Equal) => format!("something inside {name} was edited"),
            Diff::Unreadable => format!("something inside {name} can't be read any more"),
            Diff::Unreachable(e) => format!("{name} can't be checked ({e})"),
        }
    }
}

/// Everything inside a folder, in one order-independent sum: each entry's path below the folder,
/// type, size, mtime and identity, hashed and added up. Counts are kept to say what changed and
/// to stop a re-check early once it has seen more than there were.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Tree {
    items: u64,
    bytes: u64,
    sum: u64,
}

impl Tree {
    /// No limit, for `walk`.
    const ALL: Tree = Tree { items: u64::MAX, bytes: 0, sum: 0 };

    fn add(&mut self, rel: &Path, p: &Print) {
        let mut h = std::hash::DefaultHasher::new();
        (rel, p.kind, p.size, p.mtime, p.id).hash(&mut h);
        self.items += 1;
        self.bytes += p.size;
        self.sum = self.sum.wrapping_add(h.finish());
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
    /// Each of `folders` and its canonical path (links resolved), so a search through a
    /// symlink to one of them knows them by its own paths.
    pub canon: Vec<(PathBuf, PathBuf)>,
    /// The folders the operation may have changed, with their identities (links followed), so
    /// the panes showing them, by any path, read them again.
    pub folders: Vec<(PathBuf, Option<noclobber::FileId>)>,
}

pub struct Running {
    pub verb: &'static str,
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
}

type TrashFn = fn(&Path) -> Result<Option<PathBuf>, TrashError>;

/// The trash [`start`] uses: the platform's, or in tests a `.trash` folder beside each item, so
/// no test fills the user's own Trash.
#[cfg(not(test))]
const TRASH: TrashFn = crate::trash::trash;
#[cfg(test)]
const TRASH: TrashFn = tests::fake_trash;

/// Runs `op` on a worker thread; its outcome comes back through [`Running::take`].
///
/// With `dest_id`, a copy or move goes only into the folder the user chose as the one with that
/// identity (a paste, asked for before the clipboard answered): if the folder at its `dest`
/// isn't that one when the worker starts (renamed away and another made at its path, or gone),
/// it refuses before touching anything.
pub fn start(op: Op, dest_id: Option<noclobber::FileId>, waker: Option<rune::Waker>) -> Running {
    let progress = Arc::new(Progress::default());
    let done = Arc::new(Mutex::new(None));
    let verb = op.verb();
    let (p, d) = (progress.clone(), done.clone());
    // Headless runs (scripts, snapshots) run on a virtual clock far faster than real time, so
    // they do the work inline to stay deterministic.
    if rune::tooling::headless_run().is_some() {
        let outcome = Worker::new(&p, None, TRASH).run_into(op, dest_id);
        *done.lock().expect("fresh mutex") = Some(outcome);
        return Running { verb, progress, done };
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
    Running { verb, progress, done }
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
    mounts: Option<crate::delete::Mounts>,
}

struct Worker<'a> {
    p: &'a Progress,
    waker: Option<&'a rune::Waker>,
    last_wake: Instant,
    trash: TrashFn,
    /// Each copy's, move's and trash's sources as they were when it began (or, for search
    /// results, as the walk found them): checked again right before each item is acted on, so
    /// one replaced or removed while an earlier one was being handled is left alone
    /// ([`Worker::changed`]).
    seen: std::collections::HashMap<PathBuf, crate::fs::Ident>,
    /// `seen` came from a search's walk (its words name the search).
    seen_by_search: bool,
    /// Where other disks are mounted, read once per operation when a recursive removal first
    /// needs it, so none crosses into one.
    mounts: std::cell::OnceCell<crate::delete::Mounts>,
    #[cfg(test)]
    hooks: Hooks,
}

/// Why a move didn't happen.
enum MoveError {
    /// Something is at the destination. Nothing changed.
    Taken,
    /// Nothing changed (a half-made copy was removed again).
    Failed(String),
    /// Across disks: everything was copied to the destination, but part of the original
    /// couldn't be removed (`why`). Both halves fingerprinted as they are now.
    Split { split: Box<Split>, why: String },
}

enum CopyError {
    /// Something is at the destination. Nothing was made.
    Taken,
    Failed(String),
}

/// An item a copy made, and what it was copied from.
struct Made {
    src: PathBuf,
    dst: PathBuf,
    dir: bool,
    /// For a move, the source file or link as it was copied, so the delete afterwards removes
    /// it only if it hasn't changed since. Folders are removed only once empty.
    was: Option<Print>,
    /// `dst` right after the copy made it, so cleaning up a failed or cancelled copy removes it
    /// only while it's still that (see [`unmake`]). `None` if it couldn't be read back.
    made: Option<Print>,
}

impl<'a> Worker<'a> {
    fn new(p: &'a Progress, waker: Option<&'a rune::Waker>, trash: TrashFn) -> Self {
        Worker {
            p,
            waker,
            last_wake: Instant::now(),
            trash,
            seen: std::collections::HashMap::new(),
            seen_by_search: false,
            mounts: std::cell::OnceCell::new(),
            #[cfg(test)]
            hooks: Hooks::default(),
        }
    }

    /// The mount table (read on first use, then kept for this operation).
    fn mounts(&self) -> &crate::delete::Mounts {
        self.mounts.get_or_init(|| {
            #[cfg(test)]
            if let Some(m) = &self.hooks.mounts {
                return m.clone();
            }
            crate::delete::Mounts::read()
        })
    }

    fn cancelled(&self) -> bool {
        self.p.cancel.load(Relaxed)
    }

    /// Pushes progress to the UI at most ~20 times a second. The worker wakes the window;
    /// the window never polls.
    fn tick(&mut self, bytes: u64) {
        self.p.bytes_done.fetch_add(bytes, Relaxed);
        if self.last_wake.elapsed() >= Duration::from_millis(50) {
            self.last_wake = Instant::now();
            if let Some(w) = self.waker {
                w();
            }
        }
    }

    #[cfg(test)]
    fn before_act(&mut self, dest: &Path) {
        if let Some(h) = &mut self.hooks.before_act {
            h(dest);
        }
    }

    #[cfg(not(test))]
    #[inline(always)]
    fn before_act(&mut self, _: &Path) {}

    #[cfg(test)]
    fn cancel_hook(&self, at: &Path) {
        if self.hooks.cancel_at.as_ref().is_some_and(|f| f(at)) {
            self.p.cancel.store(true, Relaxed);
        }
    }

    #[cfg(not(test))]
    #[inline(always)]
    fn cancel_hook(&self, _: &Path) {}

    #[cfg(test)]
    fn after_made(&mut self, dst: &Path) {
        if let Some(h) = &mut self.hooks.after_made {
            h(dst);
        }
    }

    #[cfg(not(test))]
    #[inline(always)]
    fn after_made(&mut self, _: &Path) {}

    #[cfg(test)]
    fn copied(&mut self, dst: &Path) {
        if let Some(h) = &mut self.hooks.copied {
            h(dst);
        }
    }

    #[cfg(not(test))]
    #[inline(always)]
    fn copied(&mut self, _: &Path) {}

    fn rename_excl(&self, from: &Path, to: &Path) -> io::Result<()> {
        #[cfg(test)]
        if self.hooks.cross_device {
            return Err(io::Error::from(ErrorKind::CrossesDevices));
        }
        #[cfg(test)]
        if self.hooks.fail_rename.as_ref().is_some_and(|f| f(to)) {
            return Err(io::Error::new(ErrorKind::PermissionDenied, "permission denied"));
        }
        noclobber::rename(from, to)
    }

    /// Removes one file or link, or one empty folder.
    fn remove_one(&self, p: &Path, dir: bool) -> io::Result<()> {
        #[cfg(test)]
        if self.hooks.fail_remove.as_ref().is_some_and(|f| f(p)) {
            return Err(io::Error::new(ErrorKind::PermissionDenied, "permission denied"));
        }
        if dir { std::fs::remove_dir(p) } else { std::fs::remove_file(p) }
    }

    /// [`Worker::run`], first checking that a copy's or move's destination is still the folder
    /// with identity `dest_id`.
    fn run_into(&mut self, op: Op, dest_id: Option<noclobber::FileId>) -> Outcome {
        if let (Some(want), Op::Copy { dest, .. } | Op::Move { dest, .. }) = (dest_id, &op)
            && noclobber::dir_id(dest) != Some(want)
        {
            let why = if dest.symlink_metadata().is_ok() {
                "is another folder now (the one there was moved or replaced)"
            } else {
                "isn't there any more"
            };
            return Outcome {
                results: Vec::new(),
                undo: None,
                errors: Vec::new(),
                refused: Some(format!("Nothing was pasted: “{}” {why}", name_of(dest))),
                cancelled: false,
                moved: Vec::new(),
                removed: Vec::new(),
                no_trash: Vec::new(),
                no_trash_why: None,
                partly: None,
                canon: Vec::new(),
                folders: Vec::new(),
            };
        }
        self.run(op)
    }

    fn run(&mut self, op: Op) -> Outcome {
        // Identities from before: an operation never replaces the folders it works in.
        let folders: Vec<(PathBuf, Option<noclobber::FileId>)> = op
            .folders()
            .into_iter()
            .map(|p| {
                let id = noclobber::dir_id(&p);
                (p, id)
            })
            .collect();
        let canon = folders.iter().filter_map(|(p, _)| Some((p.clone(), std::fs::canonicalize(p).ok()?))).collect();
        if let Op::Copy { sources, .. } | Op::Move { sources, .. } | Op::Trash { sources } = &op {
            for src in sources {
                if !self.seen.contains_key(src)
                    && let Some(id) = src.symlink_metadata().ok().as_ref().and_then(crate::fs::ident)
                {
                    self.seen.insert(src.clone(), id);
                }
            }
        }
        let mut out = Outcome {
            results: Vec::new(),
            undo: None,
            errors: Vec::new(),
            refused: None,
            cancelled: false,
            moved: Vec::new(),
            removed: Vec::new(),
            no_trash: Vec::new(),
            no_trash_why: None,
            partly: None,
            canon,
            folders,
        };
        match op {
            Op::Copy { sources, dest } => {
                self.p.items_total.store(sources.len(), Relaxed);
                self.p.bytes_total.store(sources.iter().map(|s| tree_size(s)).sum(), Relaxed);
                let mut steps = Vec::new();
                for src in &sources {
                    if self.cancelled() {
                        break;
                    }
                    if let Some(why) = self.changed(src) {
                        out.errors.push(why);
                        self.p.items_done.fetch_add(1, Relaxed);
                        continue;
                    }
                    match self.copy_one(src, &dest) {
                        Ok((to, print)) => {
                            out.results.push(to.clone());
                            steps.push(Step { at: to, print, back: None, split: None });
                        }
                        Err(e) => out.errors.push(format!("{}: {e}", name_of(src))),
                    }
                    self.p.items_done.fetch_add(1, Relaxed);
                }
                let dup = sources.iter().all(|s| s.parent() == Some(dest.as_path()));
                if !steps.is_empty() {
                    out.undo = Some(Undo { what: if dup { What::Duplicate } else { What::Copy }, steps });
                }
            }
            Op::Move { sources, dest } => {
                self.p.items_total.store(sources.len(), Relaxed);
                let mut pairs = Vec::new();
                for src in &sources {
                    if self.cancelled() {
                        break;
                    }
                    if let Some(why) = self.changed(src) {
                        out.errors.push(why);
                        self.p.items_done.fetch_add(1, Relaxed);
                        continue;
                    }
                    match self.move_one(src, &dest) {
                        Ok(to) => {
                            if &to != src {
                                pairs.push((to.clone(), Some(src.clone())));
                                out.moved.push((src.clone(), to.clone()));
                            }
                            out.results.push(to);
                        }
                        Err(e) => out.errors.push(format!("{}: {e}", name_of(src))),
                    }
                    self.p.items_done.fetch_add(1, Relaxed);
                }
                out.undo = Undo::record_or_say(What::Move, pairs, &mut out.errors);
            }
            Op::Trash { sources } => {
                self.p.items_total.store(sources.len(), Relaxed);
                let mut pairs = Vec::new();
                let mut restorable = true;
                for src in &sources {
                    if self.cancelled() {
                        break;
                    }
                    if let Some(why) = self.changed(src) {
                        out.errors.push(why);
                        self.p.items_done.fetch_add(1, Relaxed);
                        continue;
                    }
                    match (self.trash)(src) {
                        Ok(Some(at)) => {
                            pairs.push((at, Some(src.clone())));
                            out.removed.push(src.clone());
                        }
                        Ok(None) => {
                            restorable = false;
                            out.removed.push(src.clone());
                        }
                        // Left where it was: gathered for one "delete permanently?" question.
                        Err(TrashError::NoTrash(why)) => {
                            out.no_trash_why.get_or_insert(why);
                            out.no_trash.push(Doomed::now(src));
                        }
                        Err(TrashError::DeletedForGood) => {
                            restorable = false;
                            out.removed.push(src.clone());
                            out.errors.push(format!("{}: {}", name_of(src), TrashError::DeletedForGood));
                        }
                        Err(TrashError::Failed(e)) => out.errors.push(format!("{}: {e}", name_of(src))),
                    }
                    self.p.items_done.fetch_add(1, Relaxed);
                    self.tick(0);
                }
                if restorable {
                    out.undo = Undo::record_or_say(What::Trash, pairs, &mut out.errors);
                }
            }
            Op::Delete { items } => self.delete(items, &mut out),
            Op::Rename { items } => self.batch_rename(items, &mut out),
            Op::Undo(undo) if undo.what == What::BatchRename => self.undo_batch_rename(undo, &mut out),
            Op::Undo(undo) => self.undo(undo, &mut out),
            Op::Checked { expect, op } => return self.run_checked(&expect, *op),
        }
        out.cancelled = self.cancelled();
        out
    }

    /// [`Op::Checked`]: leaves out each item that isn't the one the search found, says why,
    /// and runs the rest.
    fn run_checked(&mut self, expect: &[(PathBuf, crate::fs::Ident)], op: Op) -> Outcome {
        let mut why = Vec::new();
        let mut left_out = std::collections::HashSet::new();
        for (path, id) in expect {
            let name = name_of(path);
            let problem = match path.symlink_metadata() {
                Ok(m) if crate::fs::ident(&m) == Some(*id) => continue,
                Ok(_) => format!("“{name}” isn't the item the search found (something else is at its path now), so it was left alone"),
                Err(_) => format!("“{name}” isn't where the search found it any more, so it was left alone"),
            };
            why.push(problem);
            left_out.insert(path.as_path());
        }
        let (op, any) = op.without(&left_out);
        // Each item is checked again right before it's acted on (`Worker::changed`).
        self.seen.extend(expect.iter().cloned());
        self.seen_by_search = true;
        if !any {
            let more = if why.len() > 1 { format!(" (and {} more)", why.len() - 1) } else { String::new() };
            return Outcome {
                results: Vec::new(),
                undo: None,
                errors: Vec::new(),
                refused: Some(format!("Nothing was done: {}{more}", why.first().cloned().unwrap_or_default())),
                cancelled: false,
                moved: Vec::new(),
                removed: Vec::new(),
                no_trash: Vec::new(),
                no_trash_why: None,
                partly: None,
                canon: Vec::new(),
                folders: Vec::new(),
            };
        }
        let mut out = self.run(op);
        why.append(&mut out.errors);
        out.errors = why;
        out
    }

    /// Why `src` isn't the item it was when the operation began (or when the search found
    /// it), checked right before it's acted on; `None` when it still is, or there's nothing to
    /// compare with. One lstat.
    fn changed(&self, src: &Path) -> Option<String> {
        let want = self.seen.get(src)?;
        let name = name_of(src);
        Some(match (src.symlink_metadata(), self.seen_by_search) {
            (Ok(m), _) if crate::fs::ident(&m) == Some(*want) => return None,
            (Ok(_), true) => format!("“{name}” isn't the item the search found (something else is at its path now), so it was left alone"),
            (Err(_), true) => format!("“{name}” isn't where the search found it any more, so it was left alone"),
            (Ok(_), false) => format!("“{name}” was replaced while the operation ran, so it was left alone"),
            (Err(_), false) => format!("“{name}” was moved or deleted while the operation ran, so it was left alone"),
        })
    }

    /// [`Op::Delete`]: each item is checked right before it goes (an item replaced or gone since
    /// the user chose it is left alone and said so), then removed whole. A cancel stops at once:
    /// what's gone stays gone, and a folder it stopped in is partly deleted, which the outcome
    /// says ([`Outcome::partly`]).
    fn delete(&mut self, items: Vec<Doomed>, out: &mut Outcome) {
        self.p.items_total.store(items.len(), Relaxed);
        for d in &items {
            if self.cancelled() {
                break;
            }
            let name = name_of(&d.path);
            self.before_act(&d.path);
            let problem = match d.path.symlink_metadata() {
                _ if d.path.parent().is_none() => Some(format!("“{}” can't be deleted", d.path.display())),
                Err(_) => Some(format!("“{name}” was already gone")),
                Ok(m) if d.id.is_some() && crate::fs::ident(&m) != d.id => {
                    Some(format!("“{name}” isn't the item you chose (something else is at its path now), so it was left alone"))
                }
                Ok(_) => None,
            };
            if let Some(why) = problem {
                out.errors.push(why);
                self.p.items_done.fetch_add(1, Relaxed);
                continue;
            }
            let mut removed = 0;
            let mounts = self.mounts().clone();
            let w = &mut *self;
            let mut go = |at: &Path| {
                w.cancel_hook(at);
                w.tick(0);
                !w.cancelled()
            };
            match crate::delete::remove_tree(&d.path, &mounts, &mut go, &mut removed) {
                Ok(()) => out.removed.push(d.path.clone()),
                Err(crate::delete::Halt::Cancelled) => {
                    if removed > 0 {
                        out.partly = Some(d.path.clone());
                    }
                }
                Err(halt) => {
                    let Some((at, why)) = halt.why() else { continue };
                    let shown = d.path.parent().and_then(|p| at.strip_prefix(p).ok()).unwrap_or(at).display().to_string();
                    let first = if matches!(halt, crate::delete::Halt::Failed { .. }) { "Couldn't delete" } else { "Skipped" };
                    if removed == 0 {
                        out.errors.push(format!("{first} “{shown}”: {why}"));
                    } else {
                        // Truthful counts: what went, and what's still there (counted on this
                        // worker, not entering mounts; left out if a cancel stops the count).
                        let left = crate::delete::count_left(&d.path, &mounts, &mut || !self.cancelled())
                            .map_or_else(String::new, |n| format!(", {n} left"));
                        out.errors.push(format!(
                            "Partly deleted “{}”: stopped at “{shown}” ({why}); {removed} item{} deleted{left}",
                            name_of(&d.path),
                            if removed == 1 { "" } else { "s" }
                        ));
                    }
                }
            }
            self.p.items_done.fetch_add(1, Relaxed);
        }
    }

    /// Checks every step first and changes nothing unless all pass (no partial undo of a
    /// changed disk). Then undoes them one by one, checking each again right before its own
    /// step, and keeps the steps that don't go back in `out.undo`.
    fn undo(&mut self, undo: Undo, out: &mut Outcome) {
        self.p.items_total.store(undo.steps.len(), Relaxed);
        let problems: Vec<String> = undo.steps.iter().filter_map(|s| self.check(undo.what, s).err()).collect();
        if self.cancelled() {
            out.undo = Some(undo);
            return;
        }
        if let Some(first) = problems.first() {
            let more = if problems.len() > 1 { format!(" (and {} more)", problems.len() - 1) } else { String::new() };
            out.refused = Some(format!("Can't undo the {}: {first}{more}. Nothing was changed.", undo.what.noun()));
            out.undo = Some(undo);
            return;
        }
        let Undo { what, steps } = undo;
        // With one step, the check above ran just now; with more, the others' checks (walks,
        // possibly) ran in between, so each is checked again right before it's undone.
        let recheck = steps.len() > 1;
        let mut left = Vec::new();
        for step in steps {
            if self.cancelled() {
                left.push(step);
                continue;
            }
            if recheck && let Err(why) = self.check(what, &step) {
                out.errors.push(format!("Not undone: {why}"));
                left.push(step);
                continue;
            }
            if let Some(kept) = self.undo_step(what, step, out) {
                left.push(kept);
            }
            self.p.items_done.fetch_add(1, Relaxed);
            self.tick(0);
        }
        if !left.is_empty() {
            out.undo = Some(Undo { what, steps: left });
        }
    }

    /// Undoes one checked step. Hands it back (updated if it got halfway) when it didn't go.
    fn undo_step(&mut self, what: What, step: Step, out: &mut Outcome) -> Option<Step> {
        let name = name_of(&step.at);
        let Some(back) = step.back.clone() else {
            self.before_act(&step.at);
            return match (self.trash)(&step.at) {
                Ok(_) => {
                    out.removed.push(step.at.clone());
                    out.results.push(step.at);
                    None
                }
                Err(e) => {
                    out.errors.push(format!("{name}: {e}"));
                    Some(step)
                }
            };
        };
        self.before_act(&back);
        let moved = match &step.split {
            // An earlier try copied it back across disks; remove what's left of the original.
            Some(split) => self.remove_rest(&step.at, &split.copy),
            None => self.relocate(&step.at, &back),
        };
        match moved {
            Ok(()) => {
                if what == What::Trash {
                    crate::trash::forget(&step.at);
                }
                out.moved.push((step.at.clone(), back.clone()));
                out.results.push(back);
                None
            }
            Err(MoveError::Taken) => {
                out.errors.push(format!("“{}” was taken just now, so “{name}” stayed where it is", back.display()));
                Some(step)
            }
            Err(MoveError::Failed(e)) => {
                out.errors.push(format!("{name}: {e}"));
                Some(step)
            }
            Err(MoveError::Split { split, why }) => {
                out.errors.push(format!(
                    "“{name}” is back in “{}”, but part of the original couldn't be removed from “{}” ({why}). Undo again to remove the rest once that's fixed",
                    parent_of(&back),
                    parent_of(&step.at),
                ));
                Some(Step { split: Some(*split), ..step })
            }
        }
    }

    /// Whether `step` can be undone: its item is still exactly as the operation left it, and
    /// where it goes back to is free. Says what changed when not.
    fn check(&self, what: What, step: &Step) -> Result<(), String> {
        let name = format!("“{}”", name_of(&step.at));
        if let (Some(split), Some(back)) = (&step.split, &step.back) {
            return self.check_split(&name, step, split, back);
        }
        match self.diff(&step.at, &step.print) {
            None => {}
            Some(Diff::Gone) if what == What::Trash => return Err(format!("{name} is no longer in the Trash")),
            Some(Diff::Gone) if step.at.parent().is_some_and(|p| p.symlink_metadata().is_err()) => {
                return Err(format!("the folder {name} was in is gone (moved, deleted, or its drive was ejected)"));
            }
            Some(d) => return Err(d.say(&name, step.print.kind)),
        }
        if self.cancelled() {
            return Ok(()); // the caller sees the cancel and changes nothing
        }
        if let Some(back) = &step.back {
            if back.symlink_metadata().is_ok() && !noclobber::same_item(&step.at, back) {
                return Err(format!("“{}” is already taken, so {name} can't go back", back.display()));
            }
            if let Some(parent) = back.parent().filter(|p| !p.is_dir()) {
                return Err(format!("the folder it came from, “{}”, is gone", parent.display()));
            }
        }
        Ok(())
    }

    /// For a step an earlier try left split across disks: the copy at `back` and the rest at
    /// `at` must both be as that try left them (the rest may have been removed since).
    fn check_split(&self, name: &str, step: &Step, split: &Split, back: &Path) -> Result<(), String> {
        let (here, there) = (parent_of(&step.at), parent_of(back));
        if self.diff(back, &split.copy).is_some() {
            return Err(format!(
                "{name} was copied back to “{there}”, but that copy changed since, and part of the original is still in “{here}”"
            ));
        }
        match self.diff(&step.at, &split.rest) {
            None | Some(Diff::Gone) => Ok(()),
            Some(Diff::Unreachable(e)) => Err(format!(
                "{name} is back in “{there}”, but what's left of the original in “{here}” can't be checked ({e}), so it was left alone. Undo again once it can be read"
            )),
            Some(_) => {
                Err(format!("{name} is back in “{there}”, but what's left of the original in “{here}” changed since, so it was left alone"))
            }
        }
    }

    /// How `path` differs from `was` (including everything inside when `was` has a tree).
    fn diff(&self, path: &Path, was: &Print) -> Option<Diff> {
        let m = match path.symlink_metadata() {
            Ok(m) => m,
            Err(e) if is_gone(&e) => return Some(Diff::Gone),
            Err(e) => return Some(Diff::Unreachable(e)),
        };
        let now = Print::of(path, &m);
        if now.kind != was.kind {
            return Some(Diff::Kind(now.kind));
        }
        if now.id != was.id {
            return Some(Diff::Replaced);
        }
        if now.size != was.size || now.mtime != was.mtime {
            return Some(Diff::Edited);
        }
        let tree = was.tree.as_ref()?;
        let mut seen = Tree::default();
        let read = self.walk(path, Path::new(""), tree, &mut seen);
        if self.cancelled() {
            return None; // the caller sees the cancel and changes nothing
        }
        match read {
            Err(_) => Some(Diff::Unreadable),
            Ok(()) if seen != *tree => Some(Diff::Inside(seen.items.cmp(&tree.items))),
            Ok(()) => None,
        }
    }

    /// Adds everything under `dir` to `seen`, stopping once it has seen more than `cap` has.
    fn walk(&self, dir: &Path, rel: &Path, cap: &Tree, seen: &mut Tree) -> io::Result<()> {
        for de in std::fs::read_dir(dir)? {
            if self.cancelled() || seen.items > cap.items {
                return Ok(());
            }
            let de = de?;
            let path = de.path();
            let m = path.symlink_metadata()?;
            let rel = rel.join(de.file_name());
            seen.add(&rel, &Print::of(&path, &m));
            if m.is_dir() {
                self.walk(&path, &rel, cap, seen)?;
            }
        }
        Ok(())
    }

    /// `path`'s fingerprint, with everything inside when it's a folder.
    fn print_all(&self, path: &Path) -> io::Result<Print> {
        let mut p = Print::of(path, &path.symlink_metadata()?);
        if p.kind == Kind::Folder {
            let mut t = Tree::default();
            self.walk(path, Path::new(""), &Tree::ALL, &mut t)?;
            p.tree = Some(t);
        }
        Ok(p)
    }

    /// Copies `src` into `dest_dir` under a free name; returns where it went and its fingerprint.
    fn copy_one(&mut self, src: &Path, dest_dir: &Path) -> Result<(PathBuf, Print), String> {
        let name = src.file_name().ok_or("no file name")?;
        if src.is_dir() && dest_dir.starts_with(src) {
            return Err("can't copy a folder into itself".into());
        }
        let same_dir = src.parent() == Some(dest_dir);
        for _ in 0..TRIES {
            let to = free_name(dest_dir, name, same_dir);
            self.before_act(&to);
            let mut tree = Some(Tree::default());
            let mut made = Vec::new();
            match self.copy_tree(src, &to, Path::new(""), &mut tree, &mut made, false) {
                Ok(()) => {
                    // The record undo checks against: a file as the copy knew it (by its handle,
                    // never by its name, which another app may have taken over while the copy
                    // stalled); a folder read now that its contents are in, but only while it's
                    // still the folder the copy made.
                    let first = made.first().and_then(|m| m.made.clone());
                    let print = match first {
                        Some(file) if file.kind != Kind::Folder => file,
                        made_dir => {
                            let mut print = Print::of(&to, &to.symlink_metadata().map_err(|e| e.to_string())?);
                            if made_dir.is_none_or(|d| d.id.is_none() || d.id != print.id) {
                                let why = format!("something else took the place of “{}” during the copy", name_of(&to));
                                return Err(with_kept(why, unmake(&made)));
                            }
                            print.tree = tree;
                            print
                        }
                    };
                    return Ok((to, print));
                }
                // Taken since `free_name` looked: try the next free name.
                Err(CopyError::Taken) => {}
                Err(CopyError::Failed(e)) => {
                    // Don't leave a half-made copy behind.
                    return Err(with_kept(e, unmake(&made)));
                }
            }
        }
        Err("every free name was taken before it could be used".into())
    }

    /// Copies `src` to `dst`, never over anything. Each item made is pushed to `made` as it
    /// lands; with `for_move`, with the source's fingerprint. With `tree`, adds each item made
    /// below the top (`rel` is its path from the top), which costs one `lstat` per item.
    fn copy_tree(
        &mut self,
        src: &Path,
        dst: &Path,
        rel: &Path,
        tree: &mut Option<Tree>,
        made: &mut Vec<Made>,
        for_move: bool,
    ) -> Result<(), CopyError> {
        if self.cancelled() {
            return Err(CopyError::Failed("cancelled".into()));
        }
        let top = rel.as_os_str().is_empty();
        let fail = |e: io::Error| match e.kind() {
            ErrorKind::AlreadyExists if top => CopyError::Taken,
            ErrorKind::AlreadyExists => CopyError::Failed(format!("something else appeared at “{}” during the copy", dst.display())),
            _ => CopyError::Failed(e.to_string()),
        };
        let meta = src.symlink_metadata().map_err(fail)?;
        let ft = meta.file_type();
        let was = (for_move && !ft.is_dir()).then(|| Print::of(src, &meta));
        let print = |dst: &Path| dst.symlink_metadata().ok().map(|m| Print::of(dst, &m));
        if ft.is_symlink() {
            let target = std::fs::read_link(src).map_err(fail)?;
            make_symlink(&target, dst, src.is_dir()).map_err(fail)?;
            made.push(Made { src: src.into(), dst: dst.into(), dir: false, was, made: print(dst) });
        } else if ft.is_dir() {
            std::fs::create_dir(dst).map_err(fail)?;
            let _ = std::fs::set_permissions(dst, meta.permissions());
            made.push(Made { src: src.into(), dst: dst.into(), dir: true, was, made: print(dst) });
            self.after_made(dst);
            for de in std::fs::read_dir(src).map_err(fail)? {
                let de = de.map_err(fail)?;
                let name = de.file_name();
                let rel = if tree.is_some() { rel.join(&name) } else { rel.join("_") };
                self.copy_tree(&de.path(), &dst.join(&name), &rel, tree, made, for_move)?;
            }
        } else {
            // Fingerprinted from the handle that made it, not by name: the copy may have stalled
            // on a slow source while another app put a file of its own at `dst`, which cleanup
            // must then leave alone.
            let file = noclobber::copy_file(src, dst).map_err(fail)?;
            self.copied(dst);
            made.push(Made { src: src.into(), dst: dst.into(), dir: false, was, made: Some(Print::with_id(&file.meta, file.id)) });
            self.tick(meta.len());
            self.after_made(dst);
        }
        // A folder's own entry goes in after its contents, once its mtime has settled.
        if let Some(t) = tree
            && !top
        {
            t.add(rel, &Print::of(dst, &dst.symlink_metadata().map_err(fail)?));
        }
        Ok(())
    }

    fn move_one(&mut self, src: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
        let name = src.file_name().ok_or("no file name")?;
        if src.parent() == Some(dest_dir) {
            return Ok(src.to_path_buf()); // already there
        }
        if src.is_dir() && dest_dir.starts_with(src) {
            return Err("can't move a folder into itself".into());
        }
        for _ in 0..TRIES {
            let to = free_name(dest_dir, name, false);
            self.before_act(&to);
            match self.relocate(src, &to) {
                Ok(()) => return Ok(to),
                // Taken since `free_name` looked: try the next free name.
                Err(MoveError::Taken) => {}
                Err(MoveError::Failed(e)) => return Err(e),
                Err(MoveError::Split { why, .. }) => {
                    return Err(format!("copied to “{}”, but part of the original couldn't be removed ({why})", to.display()));
                }
            }
        }
        Err("every free name was taken before it could be used".into())
    }

    /// Moves `from` to `to`, never over anything: a no-replace rename, or across disks a copy
    /// and then a delete of exactly what was copied.
    fn relocate(&mut self, from: &Path, to: &Path) -> Result<(), MoveError> {
        match self.rename_excl(from, to) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => Err(MoveError::Taken),
            Err(e) if noclobber::is_cross_device(&e) => self.copy_then_remove(from, to),
            Err(e) => Err(MoveError::Failed(e.to_string())),
        }
    }

    /// Across disks: copies `from` to `to`, then removes the original. If the copy fails, what
    /// it made is removed and nothing has changed. If the delete fails partway, everything is at
    /// `to` and the rest stays at `from`: [`MoveError::Split`] says so, fingerprinting both.
    fn copy_then_remove(&mut self, from: &Path, to: &Path) -> Result<(), MoveError> {
        self.p.bytes_total.fetch_add(tree_size(from), Relaxed);
        let mut tree = Some(Tree::default());
        let mut made = Vec::new();
        match self.copy_tree(from, to, Path::new(""), &mut tree, &mut made, true) {
            Ok(()) => {}
            Err(CopyError::Taken) => return Err(MoveError::Taken),
            Err(CopyError::Failed(e)) => return Err(MoveError::Failed(with_kept(e, unmake(&made)))),
        }
        let Err(why) = self.remove_sources(&made) else { return Ok(()) };
        let copy = match to.symlink_metadata() {
            Ok(m) => Print { tree: tree.filter(|_| m.is_dir()), ..Print::of(to, &m) },
            Err(e) => return Err(MoveError::Failed(format!("copied, then the copy vanished: {e}"))),
        };
        self.split(from, copy, why)
    }

    /// After a failed delete, fingerprints what's left at `from` so a retry can finish.
    fn split(&self, from: &Path, copy: Print, why: String) -> Result<(), MoveError> {
        match self.print_all(from) {
            Ok(rest) => Err(MoveError::Split { split: Box::new(Split { copy, rest }), why }),
            // The rest is gone after all.
            Err(e) if is_gone(&e) => Ok(()),
            // Unreadable: a fingerprint nothing matches, so a retry refuses and says where things are.
            Err(e) => Err(MoveError::Split { split: Box::new(Split { copy, rest: unmatchable() }), why: format!("{why}; {e}") }),
        }
    }

    /// Removes the originals of a cross-disk move, deepest first: each file or link only if
    /// it's unchanged since it was copied, each folder only once it's empty. Anything that
    /// changed or appeared meanwhile stays. Says what couldn't be removed.
    fn remove_sources(&self, made: &[Made]) -> Result<(), String> {
        let (mut kept, mut first) = (0, None);
        // The first item made is the moved item itself: nothing on another disk mounted inside
        // it is removed (it was copied; the original stays where it's mounted).
        let fence = made.first().map(|m| crate::delete::Fence::new(&m.src, self.mounts()));
        for m in made.iter().rev() {
            if let (Some(f), Ok(meta)) = (&fence, m.src.symlink_metadata())
                && f.crosses(&m.src, &meta)
            {
                // Counted and named once, at its mount point (what's in it comes first, deepest
                // first, and is left alone without a word).
                let parent_crosses = m.src.parent().and_then(|p| p.symlink_metadata().ok().map(|pm| f.crosses(p, &pm)));
                if parent_crosses == Some(false) {
                    kept += 1;
                    first.get_or_insert_with(|| format!("“{}”: another disk is mounted there", m.src.display()));
                }
                continue;
            }
            let removed = match &m.was {
                Some(was) => match m.src.symlink_metadata() {
                    Ok(now) if Print::of(&m.src, &now) == *was => self.remove_one(&m.src, false),
                    Ok(_) => Err(io::Error::other("it changed during the move")),
                    Err(e) => Err(e),
                },
                None => self.remove_one(&m.src, m.dir),
            };
            match removed {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                // A folder that keeps something kept below it: that one's already counted.
                Err(_) if m.dir && first.is_some() => {}
                Err(e) => {
                    kept += 1;
                    first.get_or_insert_with(|| format!("“{}”: {e}", m.src.display()));
                }
            }
        }
        match first {
            None => Ok(()),
            Some(f) if kept == 1 => Err(f),
            Some(f) => Err(format!("{f}, and {} more", kept - 1)),
        }
    }

    /// Finishes a split step: removes what's left of the original at `at` (checked unchanged
    /// just before), deepest first, folders only once empty.
    fn remove_rest(&mut self, at: &Path, copy: &Print) -> Result<(), MoveError> {
        let mut kept = None;
        let fence = crate::delete::Fence::new(at, self.mounts());
        self.remove_tree_checked(at, &fence, &mut kept);
        match kept {
            None => Ok(()),
            Some(why) => self.split(at, copy.clone(), why),
        }
    }

    /// Only an item that's really gone counts as removed: one that can't be read (its folder
    /// lost its permissions, say) is kept, and the step's record with it. Never enters or
    /// removes a mount point (`fence`).
    fn remove_tree_checked(&self, p: &Path, fence: &crate::delete::Fence, kept: &mut Option<String>) {
        let keep = |kept: &mut Option<String>, e: io::Error| {
            kept.get_or_insert_with(|| format!("“{}”: {e}", p.display()));
        };
        let m = match p.symlink_metadata() {
            Ok(m) => m,
            Err(e) if is_gone(&e) => return,
            Err(e) => return keep(kept, e),
        };
        if fence.crosses(p, &m) {
            kept.get_or_insert_with(|| format!("“{}”: another disk is mounted there", p.display()));
            return;
        }
        if m.is_dir() {
            match std::fs::read_dir(p) {
                Ok(rd) => {
                    for de in rd {
                        match de {
                            Ok(de) => self.remove_tree_checked(&de.path(), fence, kept),
                            Err(e) => keep(kept, e),
                        }
                    }
                }
                Err(e) if is_gone(&e) => return,
                Err(e) => keep(kept, e),
            }
        }
        if let Err(e) = self.remove_one(p, m.is_dir())
            && e.kind() != ErrorKind::NotFound
            && !(m.is_dir() && kept.is_some())
        {
            *kept = Some(format!("“{}”: {e}", p.display()));
        }
    }
}

// ---- batch rename ---------------------------------------------------------------------------

/// Where one item of a batch is while it's renamed, in the order it got there.
struct Done {
    item: usize,
    now: PathBuf,
    was: PathBuf,
}

impl Worker<'_> {
    /// A batch rename. Before touching anything, every item must still be as the preview saw it
    /// and every new name free (or held by another item of the batch, which moves out of the
    /// way); otherwise nothing is renamed and it says which item and why. Then
    /// [`Worker::rename_all`]: all of it, or none of it.
    fn batch_rename(&mut self, items: Vec<Renaming>, out: &mut Outcome) {
        let moves: Vec<(PathBuf, PathBuf)> = items.iter().map(|r| (r.from.clone(), r.to.clone())).collect();
        let seen: Vec<&Print> = items.iter().map(|r| &r.seen.0).collect();
        if let Err(why) = self.check_batch(&moves, &seen) {
            out.refused = Some(format!("Nothing was renamed: {why}."));
            return;
        }
        if self.rename_all(&moves, &seen, out) {
            out.results = moves.iter().map(|(_, to)| to.clone()).collect();
            out.undo =
                Undo::record_or_say(What::BatchRename, moves.iter().map(|(from, to)| (to.clone(), Some(from.clone()))), &mut out.errors);
            out.moved = moves;
        }
    }

    /// Undoes a batch rename as one: every item must be as the batch left it and its old name
    /// free or held by another item of the batch, or nothing is undone. Then the same two-step
    /// plan, which swaps and chains need going back too. A failed or cancelled undo stays on
    /// the stack.
    fn undo_batch_rename(&mut self, undo: Undo, out: &mut Outcome) {
        let moves: Vec<(PathBuf, PathBuf)> = undo.steps.iter().filter_map(|s| Some((s.at.clone(), s.back.clone()?))).collect();
        let seen: Vec<&Print> = undo.steps.iter().map(|s| &s.print).collect();
        if let Err(why) = self.check_batch(&moves, &seen) {
            out.refused = Some(format!("Can't undo the batch rename: {why}. Nothing was changed."));
            out.undo = Some(undo);
            return;
        }
        if self.rename_all(&moves, &seen, out) {
            out.results = moves.iter().map(|(_, back)| back.clone()).collect();
            out.moved = moves;
        } else {
            out.undo = Some(undo);
        }
    }

    /// Whether every `(from, to)` can go: `from` still has fingerprint `seen`, and `to` is in
    /// the same folder and free, or is an item of the batch (which moves first). Says the first
    /// problem.
    fn check_batch(&self, moves: &[(PathBuf, PathBuf)], seen: &[&Print]) -> Result<(), String> {
        for ((from, _), was) in moves.iter().zip(seen) {
            if let Some(d) = self.diff(from, was) {
                let name = format!("“{}”", name_of(from));
                return Err(match d {
                    Diff::Unreachable(e) => format!("{name} can't be checked ({e})"),
                    d => format!("{} since the preview", d.say(&name, was.kind)),
                });
            }
        }
        // An item inside another one being renamed: the folder's rename would move it from under
        // its own step, and undo couldn't find it (the preview says so first).
        for (inner, _) in moves {
            if let Some((outer, _)) = moves.iter().find(|(o, _)| o != inner && inner.starts_with(o)) {
                return Err(format!(
                    "“{}” is inside “{}”, which is being renamed too; rename them separately",
                    name_of(inner),
                    name_of(outer)
                ));
            }
        }
        let ids: std::collections::HashSet<(u64, u64)> = seen.iter().filter_map(|p| p.id).collect();
        let froms: std::collections::HashSet<String> = moves.iter().map(|(f, _)| fold_path(f)).collect();
        for (from, to) in moves {
            if from.parent() != to.parent() || to.file_name().is_none() {
                return Err(format!("“{}” would leave its folder", name_of(from)));
            }
            let Ok(m) = to.symlink_metadata() else { continue };
            let ours = match noclobber::file_id(to, &m) {
                Some(id) => ids.contains(&id),
                // No identity to tell by: only a name the batch itself holds.
                None => froms.contains(&fold_path(to)),
            };
            if !ours {
                return Err(format!("“{}” already exists in “{}”", name_of(to), parent_of(to)));
            }
        }
        Ok(())
    }

    /// Renames each `(from, to)` (each in its own folder), never over anything, as one step
    /// that either all happens or is put back. An item whose new name is another item's old one
    /// (a swap, a chain, a case-only change) first steps aside to a temporary name beside it
    /// (`.arbor-…`, as [`noclobber::rename`]'s case-only renames do); then the others get
    /// their new names, freeing the old ones; then the ones aside get theirs. A failure or a
    /// cancel midway puts every item back, last step first, again never over anything; what
    /// can't go back (another app took its old name meanwhile, or changed the item since it was
    /// renamed) stays where it is and the message says exactly where. Each item is checked
    /// against `seen` (its fingerprint when the plan was made) right before each of its steps
    /// and before it's put back, so an item replaced or edited while the batch runs is never
    /// renamed, and a replacement is never put back in an item's place. True when every item
    /// has its new name.
    fn rename_all(&mut self, moves: &[(PathBuf, PathBuf)], seen: &[&Print], out: &mut Outcome) -> bool {
        self.p.items_total.store(moves.len(), Relaxed);
        let froms: std::collections::HashSet<String> = moves.iter().map(|(f, _)| fold_path(f)).collect();
        // Compared case aside on every disk: a needless step aside costs one rename.
        let aside: Vec<bool> = moves.iter().map(|(_, to)| froms.contains(&fold_path(to))).collect();
        let mut done: Vec<Done> = Vec::new();
        let Err(why) = self.rename_steps(moves, seen, &aside, &mut done) else { return true };
        let stuck = self.put_back(done, seen);
        let cancelled = self.cancelled();
        let lead = if cancelled { "Cancelled".to_string() } else { why };
        if stuck.is_empty() {
            out.errors.push(format!("{lead}. Nothing was renamed: every item has its old name again"));
            return false;
        }
        let at = |(item, now, why): &(usize, PathBuf, Stuck)| match why {
            Stuck::Taken => format!(
                "“{}” is now “{}” in “{}” (something else took its old name)",
                name_of(&moves[*item].0),
                name_of(now),
                parent_of(now)
            ),
            Stuck::Changed => format!(
                "“{}” in “{}” changed after it was renamed (it was “{}”), so it was left there",
                name_of(now),
                parent_of(now),
                name_of(&moves[*item].0)
            ),
        };
        let shown: Vec<String> = stuck.iter().take(3).map(at).collect();
        let more = if stuck.len() > 3 { format!(" (and {} more)", stuck.len() - 3) } else { String::new() };
        let back = moves.len() - stuck.len();
        out.errors.push(format!("{lead}. {back} of {} items have their old names again; {}{more}", moves.len(), shown.join("; ")));
        out.errors.extend(stuck.iter().skip(3).map(at));
        false
    }

    /// Whether the item at `at` (first at `orig`) is still the one the plan was made for
    /// (fingerprint `was`); says how it changed when not.
    fn step_check(&self, at: &Path, orig: &Path, was: &Print) -> Result<(), String> {
        match self.diff(at, was) {
            None => Ok(()),
            Some(Diff::Unreachable(e)) => Err(format!("“{}” can't be checked ({e})", name_of(orig))),
            Some(d) => Err(format!("{} while the batch was renaming", d.say(&format!("“{}”", name_of(orig)), was.kind))),
        }
    }

    /// The steps of [`Worker::rename_all`], recording each in `done`. The first failure (or a
    /// cancel) stops it and says why.
    fn rename_steps(&mut self, moves: &[(PathBuf, PathBuf)], seen: &[&Print], aside: &[bool], done: &mut Vec<Done>) -> Result<(), String> {
        let mut at: Vec<PathBuf> = moves.iter().map(|(from, _)| from.clone()).collect();
        for (i, (from, _)) in moves.iter().enumerate().filter(|&(i, _)| aside[i]) {
            if self.cancelled() {
                return Err("Cancelled".into());
            }
            self.step_check(from, from, seen[i])?;
            let tmp = self.step_aside(from)?;
            done.push(Done { item: i, now: tmp.clone(), was: from.clone() });
            at[i] = tmp;
            self.tick(0);
        }
        for pass in [false, true] {
            for (i, (from, to)) in moves.iter().enumerate().filter(|&(i, _)| aside[i] == pass) {
                if self.cancelled() {
                    return Err("Cancelled".into());
                }
                self.before_act(to);
                // Right before its own step: still the item the plan was made for.
                self.step_check(&at[i], from, seen[i])?;
                match self.rename_excl(&at[i], to) {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                        return Err(format!("“{}” was taken just now, so “{}” couldn't have that name", name_of(to), name_of(from)));
                    }
                    Err(e) => return Err(format!("Couldn't rename “{}” to “{}”: {e}", name_of(from), name_of(to))),
                }
                done.push(Done { item: i, now: to.clone(), was: std::mem::replace(&mut at[i], to.clone()) });
                self.p.items_done.fetch_add(1, Relaxed);
                self.tick(0);
            }
        }
        Ok(())
    }

    /// Moves `from` to a free temporary name beside it; returns the name.
    fn step_aside(&mut self, from: &Path) -> Result<PathBuf, String> {
        for _ in 0..TRIES {
            let tmp = noclobber::temp_sibling(from).map_err(|e| e.to_string())?;
            match self.rename_excl(from, &tmp) {
                Ok(()) => return Ok(tmp),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("Couldn't rename “{}”: {e}", name_of(from))),
            }
        }
        Err(format!("Couldn't find a free temporary name beside “{}”", name_of(from)))
    }

    /// Undoes `done`, last step first, never over anything, and only while what's at each
    /// step's new name is still the item that went there (`seen`): a replacement or an edited
    /// item is never moved into an item's old name. An item that can't go back stays where it is
    /// (its earlier steps aren't tried). Returns those, with where each is and why.
    fn put_back(&mut self, done: Vec<Done>, seen: &[&Print]) -> Vec<(usize, PathBuf, Stuck)> {
        let mut stuck: Vec<(usize, PathBuf, Stuck)> = Vec::new();
        for d in done.into_iter().rev() {
            if stuck.iter().any(|(i, _, _)| *i == d.item) {
                continue;
            }
            self.before_act(&d.was);
            if self.diff(&d.now, seen[d.item]).is_some() {
                stuck.push((d.item, d.now, Stuck::Changed));
            } else if self.rename_excl(&d.now, &d.was).is_err() {
                stuck.push((d.item, d.now, Stuck::Taken));
            }
        }
        stuck.sort_by_key(|(i, _, _)| *i);
        stuck
    }
}

/// Why a batch rename's rollback left an item where it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stuck {
    /// Something else took its old name.
    Taken,
    /// What's at its new name isn't the item that went there any more (replaced or edited).
    Changed,
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

/// A fingerprint nothing on disk matches.
fn unmatchable() -> Print {
    Print { kind: Kind::Link, size: u64::MAX, mtime: None, id: None, tree: None }
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

#[cfg(test)]
mod tests {
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
    pub(super) fn fake_trash(p: &Path) -> Result<Option<PathBuf>, TrashError> {
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

    fn refusing_trash(_: &Path) -> Result<Option<PathBuf>, TrashError> {
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
    fn picky_trash(p: &Path) -> Result<Option<PathBuf>, TrashError> {
        if p.file_name().unwrap().to_string_lossy().starts_with('b') {
            Err(TrashError::Failed("permission denied".into()))
        } else {
            fake_trash(p)
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
            let want =
                format!("src: cancelled; kept “{name}” in “{}”: it changed after Arbor made it", d.join("into").join("src").display());
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

    /// A permanent delete never crosses into a mounted disk: it stops at the mount point, names
    /// it, and leaves what's in it; the folders above it stay, and the counts say so.
    #[test]
    fn delete_stops_at_a_mount_point() {
        let d = tmp("del-mount");
        fs::create_dir_all(d.join("doomed/mnt")).unwrap();
        fs::write(d.join("doomed/mnt/precious"), "keep").unwrap();
        let canon = fs::canonicalize(&d).unwrap();
        let mounts = crate::delete::Mounts::of(&[&canon.join("doomed/mnt")]);
        let o =
            run_hooked(Op::Delete { items: doomed(&[d.join("doomed")]) }, fake_trash, Hooks { mounts: Some(mounts), ..Hooks::default() });
        assert!(o.removed.is_empty());
        assert_eq!(o.errors, ["Skipped “doomed/mnt”: another disk is mounted there"]);
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
        let mounts = crate::delete::Mounts::of(&[&canon.join("src/mnt")]);
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
    fn trash_then_replace_b(p: &Path) -> Result<Option<PathBuf>, TrashError> {
        let out = fake_trash(p)?;
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
        let u =
            Undo::record_or_say(What::BatchRename, [(d.join("a"), Some(d.join("x"))), (d.join("gone"), Some(d.join("y")))], &mut errors);
        assert_eq!(u.map(|u| u.steps.len()), Some(1));
        assert!(errors.len() == 1 && errors[0].starts_with("“gone” can't be undone"), "{errors:?}");
    }
}
