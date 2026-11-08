//! File operations: copy, move, trash, and undo. Each runs on its own worker thread and reports
//! progress through atomics; the UI reads them in `poll`.
//!
//! Safety rules: nothing is ever overwritten. A name that's taken gets a free one instead
//! ("name copy" in the same folder, "name 2" elsewhere). Folders can't go into themselves.
//! Undo refuses rather than guesses: every result is fingerprinted when the operation finishes,
//! and undo checks all of them before touching anything (see [`Print`]).

use std::fs::Metadata;
use std::hash::{Hash, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

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
    /// Reverse an earlier operation, if the disk still looks the way it left it.
    Undo(Undo),
}

impl Op {
    pub fn verb(&self) -> &'static str {
        match self {
            Op::Copy { .. } => "Copying",
            Op::Move { .. } => "Moving",
            Op::Trash { .. } => "Moving to Trash",
            Op::Undo(_) => "Undoing",
        }
    }
}

/// What an undo reverses, for its messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum What {
    Rename,
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
}

impl Undo {
    /// Fingerprints each `(at, back)` as it is now. Items already gone are left out.
    fn record(what: What, items: impl IntoIterator<Item = (PathBuf, Option<PathBuf>)>) -> Option<Undo> {
        let steps: Vec<Step> = items
            .into_iter()
            .filter_map(|(at, back)| {
                let print = Print::of(&at.symlink_metadata().ok()?);
                Some(Step { at, print, back })
            })
            .collect();
        (!steps.is_empty()).then_some(Undo { what, steps })
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
/// - Identity: device and inode on Unix, so a file deleted and recreated under the same name
///   reads as replaced. Windows has no stable file id in std (`file_index` is unstable), so it
///   uses the creation time, which also changes when a file is replaced (except within NTFS's
///   15-second "tunnelling" window, where size and mtime still have to match).
/// - Type, size (files and links) and modified time. A folder's own mtime changes when an
///   entry directly inside it is added, removed or renamed.
/// - `tree`: for folders whose undo trashes them (copies, duplicates, new folders), a sum over
///   everything inside. A folder's mtime doesn't see an edit deeper down, and trashing a copy the
///   user has since worked in is exactly the wrong undo this exists to stop. Moves and renames
///   don't walk: moving a folder back carries whatever is inside with it and loses nothing, so a
///   walk would cost O(tree) to protect nothing.
///
/// Not seen: an edit that keeps the size and restores the mtime exactly (that needs content
/// hashing, far too slow), and changes in the instant between the check and the undo.
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
    fn of(m: &Metadata) -> Print {
        let kind = Kind::of(m);
        Print { kind, size: if kind == Kind::Folder { 0 } else { m.len() }, mtime: m.modified().ok(), id: file_id(m), tree: None }
    }
}

#[cfg(unix)]
fn file_id(m: &Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((m.dev(), m.ino()))
}

#[cfg(windows)]
fn file_id(m: &Metadata) -> Option<(u64, u64)> {
    let d = m.created().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some((d.as_secs(), u64::from(d.subsec_nanos())))
}

#[cfg(not(any(unix, windows)))]
fn file_id(_: &Metadata) -> Option<(u64, u64)> {
    None
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
    fn add(&mut self, rel: &Path, m: &Metadata) {
        let p = Print::of(m);
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

type TrashFn = fn(&Path) -> Result<Option<PathBuf>, String>;

pub fn start(op: Op, waker: Option<rune::Waker>) -> Running {
    let progress = Arc::new(Progress::default());
    let done = Arc::new(Mutex::new(None));
    let verb = op.verb();
    let (p, d) = (progress.clone(), done.clone());
    // Headless runs (scripts, snapshots) run on a virtual clock far faster than real time, so
    // they do the work inline to stay deterministic.
    if rune::tooling::headless_run().is_some() {
        let outcome = Worker::new(&p, None, crate::trash::trash).run(op);
        *done.lock().expect("fresh mutex") = Some(outcome);
        return Running { verb, progress, done };
    }
    std::thread::spawn(move || {
        let outcome = Worker::new(&p, waker.as_ref(), crate::trash::trash).run(op);
        if let Ok(mut slot) = d.lock() {
            *slot = Some(outcome);
        }
        if let Some(wake) = &waker {
            wake();
        }
    });
    Running { verb, progress, done }
}

/// Runs an operation on this thread, trashing with `trash` (tests).
#[cfg(test)]
fn run_with(op: Op, trash: TrashFn) -> Outcome {
    let p = Progress::default();
    Worker::new(&p, None, trash).run(op)
}

struct Worker<'a> {
    p: &'a Progress,
    waker: Option<&'a rune::Waker>,
    last_wake: Instant,
    trash: TrashFn,
}

impl<'a> Worker<'a> {
    fn new(p: &'a Progress, waker: Option<&'a rune::Waker>, trash: TrashFn) -> Self {
        Worker { p, waker, last_wake: Instant::now(), trash }
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

    fn run(&mut self, op: Op) -> Outcome {
        let mut out = Outcome { results: Vec::new(), undo: None, errors: Vec::new(), refused: None, cancelled: false };
        match op {
            Op::Copy { sources, dest } => {
                self.p.items_total.store(sources.len(), Relaxed);
                self.p.bytes_total.store(sources.iter().map(|s| tree_size(s)).sum(), Relaxed);
                let mut steps = Vec::new();
                for src in &sources {
                    if self.cancelled() {
                        break;
                    }
                    match self.copy_one(src, &dest) {
                        Ok((to, print)) => {
                            out.results.push(to.clone());
                            steps.push(Step { at: to, print, back: None });
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
                    match self.move_one(src, &dest) {
                        Ok(to) => {
                            if &to != src {
                                pairs.push((to.clone(), Some(src.clone())));
                            }
                            out.results.push(to);
                        }
                        Err(e) => out.errors.push(format!("{}: {e}", name_of(src))),
                    }
                    self.p.items_done.fetch_add(1, Relaxed);
                }
                out.undo = Undo::record(What::Move, pairs);
            }
            Op::Trash { sources } => {
                self.p.items_total.store(sources.len(), Relaxed);
                let mut pairs = Vec::new();
                let mut restorable = true;
                for src in &sources {
                    if self.cancelled() {
                        break;
                    }
                    match (self.trash)(src) {
                        Ok(Some(at)) => pairs.push((at, Some(src.clone()))),
                        Ok(None) => restorable = false,
                        Err(e) => out.errors.push(format!("{}: {e}", name_of(src))),
                    }
                    self.p.items_done.fetch_add(1, Relaxed);
                    self.tick(0);
                }
                if restorable {
                    out.undo = Undo::record(What::Trash, pairs);
                }
            }
            Op::Undo(undo) => self.undo(undo, &mut out),
        }
        out.cancelled = self.cancelled();
        out
    }

    /// Checks every step first and changes nothing unless all pass (no partial undo of a
    /// changed disk). Then undoes them, keeping the steps that fail in `out.undo`.
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
        let mut left = Vec::new();
        for step in steps {
            if self.cancelled() {
                left.push(step);
                continue;
            }
            let done = match &step.back {
                Some(back) => self.rename_or_copy(&step.at, back).map(|()| back.clone()),
                None => (self.trash)(&step.at).map(|_| step.at.clone()),
            };
            match done {
                Ok(p) => {
                    if what == What::Trash {
                        crate::trash::forget(&step.at);
                    }
                    out.results.push(p);
                }
                Err(e) => {
                    out.errors.push(format!("{}: {e}", name_of(&step.at)));
                    left.push(step);
                }
            }
            self.p.items_done.fetch_add(1, Relaxed);
            self.tick(0);
        }
        if !left.is_empty() {
            out.undo = Some(Undo { what, steps: left });
        }
    }

    /// Whether `step` can be undone: its item is still exactly as the operation left it, and
    /// where it goes back to is free. Says what changed when not.
    fn check(&self, what: What, step: &Step) -> Result<(), String> {
        let name = format!("“{}”", name_of(&step.at));
        let m = match step.at.symlink_metadata() {
            Ok(m) => m,
            Err(_) if what == What::Trash => return Err(format!("{name} is no longer in the Trash")),
            Err(_) if step.at.parent().is_some_and(|p| p.symlink_metadata().is_err()) => {
                return Err(format!("the folder {name} was in is gone (moved, deleted, or its drive was ejected)"));
            }
            Err(_) => return Err(format!("{name} was moved or deleted")),
        };
        let now = Print::of(&m);
        let was = &step.print;
        if now.kind != was.kind {
            return Err(format!("{name} was replaced by {}", now.kind.name()));
        }
        if now.id != was.id {
            return Err(format!("{name} was replaced by another item with the same name"));
        }
        if now.size != was.size || now.mtime != was.mtime {
            return Err(match was.kind {
                Kind::Folder => format!("items were added, removed or renamed in {name}"),
                _ => format!("{name} was edited"),
            });
        }
        if let Some(tree) = &was.tree {
            let mut seen = Tree::default();
            let read = self.walk(&step.at, Path::new(""), tree, &mut seen);
            if self.cancelled() {
                return Ok(()); // the caller sees the cancel and changes nothing
            }
            if read.is_err() || seen != *tree {
                return Err(match seen.items.cmp(&tree.items) {
                    _ if read.is_err() => format!("something inside {name} can't be read any more"),
                    std::cmp::Ordering::Greater => format!("items were added inside {name}"),
                    std::cmp::Ordering::Less => format!("items were removed from inside {name}"),
                    std::cmp::Ordering::Equal => format!("something inside {name} was edited"),
                });
            }
        }
        if let Some(back) = &step.back {
            if let Ok(there) = back.symlink_metadata()
                && !same_file(&step.at, &m, back, &there)
            {
                return Err(format!("“{}” is already taken, so {name} can't go back", back.display()));
            }
            if let Some(parent) = back.parent().filter(|p| !p.is_dir()) {
                return Err(format!("the folder it came from, “{}”, is gone", parent.display()));
            }
        }
        Ok(())
    }

    /// Adds everything under `dir` to `seen`, stopping once it has seen more than `cap` has.
    fn walk(&self, dir: &Path, rel: &Path, cap: &Tree, seen: &mut Tree) -> std::io::Result<()> {
        for de in std::fs::read_dir(dir)? {
            if self.cancelled() || seen.items > cap.items {
                return Ok(());
            }
            let de = de?;
            let m = de.path().symlink_metadata()?;
            let rel = rel.join(de.file_name());
            seen.add(&rel, &m);
            if m.is_dir() {
                self.walk(&de.path(), &rel, cap, seen)?;
            }
        }
        Ok(())
    }

    /// Copies `src` into `dest_dir` under a free name; returns where it went and its fingerprint.
    fn copy_one(&mut self, src: &Path, dest_dir: &Path) -> Result<(PathBuf, Print), String> {
        let name = src.file_name().ok_or("no file name")?;
        if src.is_dir() && dest_dir.starts_with(src) {
            return Err("can't copy a folder into itself".into());
        }
        let same_dir = src.parent() == Some(dest_dir);
        let to = free_name(dest_dir, name, same_dir);
        let mut tree = Some(Tree::default());
        if let Err(e) = self.copy_tree(src, &to, Path::new(""), &mut tree) {
            // Don't leave a half-made copy behind.
            remove_tree(&to);
            return Err(e);
        }
        let mut print = Print::of(&to.symlink_metadata().map_err(|e| e.to_string())?);
        if print.kind == Kind::Folder {
            print.tree = tree;
        }
        Ok((to, print))
    }

    /// Copies `src` to `dst`. With `tree`, adds each item made below the top (`rel` is its path
    /// from the top) as it lands, which costs one `lstat` per item and no second walk.
    fn copy_tree(&mut self, src: &Path, dst: &Path, rel: &Path, tree: &mut Option<Tree>) -> Result<(), String> {
        if self.cancelled() {
            return Err("cancelled".into());
        }
        let meta = src.symlink_metadata().map_err(|e| e.to_string())?;
        let ft = meta.file_type();
        if ft.is_symlink() {
            let target = std::fs::read_link(src).map_err(|e| e.to_string())?;
            make_symlink(&target, dst, src.is_dir()).map_err(|e| e.to_string())?;
        } else if ft.is_dir() {
            std::fs::create_dir(dst).map_err(|e| e.to_string())?;
            let _ = std::fs::set_permissions(dst, meta.permissions());
            for de in std::fs::read_dir(src).map_err(|e| e.to_string())? {
                let de = de.map_err(|e| e.to_string())?;
                let name = de.file_name();
                let rel = if tree.is_some() { rel.join(&name) } else { PathBuf::new() };
                self.copy_tree(&de.path(), &dst.join(&name), &rel, tree)?;
            }
        } else {
            // On macOS this clones on APFS: instant, and no extra space.
            std::fs::copy(src, dst).map_err(|e| e.to_string())?;
            self.tick(meta.len());
        }
        // A folder's own entry goes in after its contents, once its mtime has settled.
        if let Some(t) = tree
            && !rel.as_os_str().is_empty()
        {
            t.add(rel, &dst.symlink_metadata().map_err(|e| e.to_string())?);
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
        let to = free_name(dest_dir, name, false);
        self.rename_or_copy(src, &to)?;
        Ok(to)
    }

    /// `rename`, or copy then delete when the destination is on another disk.
    fn rename_or_copy(&mut self, from: &Path, to: &Path) -> Result<(), String> {
        match std::fs::rename(from, to) {
            Ok(()) => Ok(()),
            Err(e) if is_cross_device(&e) => {
                self.p.bytes_total.fetch_add(tree_size(from), Relaxed);
                if let Err(e) = self.copy_tree(from, to, Path::new(""), &mut None) {
                    remove_tree(to);
                    return Err(e);
                }
                let removed = if from.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                    std::fs::remove_dir_all(from)
                } else {
                    std::fs::remove_file(from)
                };
                removed.map_err(|e| format!("copied, but couldn't remove the original: {e}"))
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

fn is_cross_device(e: &std::io::Error) -> bool {
    e.kind() == ErrorKind::CrossesDevices || (cfg!(unix) && e.raw_os_error() == Some(18)) || (cfg!(windows) && e.raw_os_error() == Some(17))
}

/// Whether `a` and `b` are the same item (a case-only rename on a case-insensitive disk).
#[cfg(unix)]
fn same_file(_: &Path, a: &Metadata, _: &Path, b: &Metadata) -> bool {
    file_id(a) == file_id(b)
}

/// Whether `a` and `b` are the same item. Without a file id in std, a case-only difference in
/// the same folder (Windows disks are case-insensitive).
#[cfg(not(unix))]
fn same_file(a: &Path, _: &Metadata, b: &Path, _: &Metadata) -> bool {
    a.parent() == b.parent() && a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
}

fn name_of(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
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

fn remove_tree(p: &Path) {
    match p.symlink_metadata() {
        Ok(m) if m.is_dir() => drop(std::fs::remove_dir_all(p)),
        Ok(_) => drop(std::fs::remove_file(p)),
        Err(_) => {}
    }
}

#[cfg(unix)]
fn make_symlink(target: &Path, at: &Path, _is_dir: bool) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, at)
}

#[cfg(windows)]
fn make_symlink(target: &Path, at: &Path, is_dir: bool) -> std::io::Result<()> {
    if is_dir { std::os::windows::fs::symlink_dir(target, at) } else { std::os::windows::fs::symlink_file(target, at) }
}

/// A free path for `name` in `dir`: the name itself if nothing's there, else
/// "stem copy.ext", "stem copy 2.ext" (same folder) or "stem 2.ext" (elsewhere).
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
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-ops-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Stands in for the platform trash: moves the item to a `.trash` folder beside it, so tests
    /// never touch the real one.
    fn fake_trash(p: &Path) -> Result<Option<PathBuf>, String> {
        let bin = p.parent().unwrap().join(".trash");
        fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
        let at = free_name(&bin, p.file_name().unwrap(), false);
        fs::rename(p, &at).map_err(|e| e.to_string())?;
        Ok(Some(at))
    }

    fn refusing_trash(_: &Path) -> Result<Option<PathBuf>, String> {
        Err("permission denied".into())
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

    /// Unix only: on Windows a file renamed onto a just-freed name inherits its creation time
    /// (NTFS tunnelling), so a replacement with the same size and mtime can't be told apart there.
    #[cfg(unix)]
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
    fn picky_trash(p: &Path) -> Result<Option<PathBuf>, String> {
        if p.file_name().unwrap().to_string_lossy().starts_with('b') { Err("permission denied".into()) } else { fake_trash(p) }
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
}
