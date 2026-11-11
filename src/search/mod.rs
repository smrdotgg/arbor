//! Recursive search (NORTHSTAR Core: "live walk, no index, streaming").
//!
//! "Search in This Folder" (⌘F) turns the active tab into a search view rooted at its folder:
//! a query field over the results, which stream in as a walk of the tree finds them. Nothing is
//! stored: every search walks again, and leaving it reads the folder fresh (as showing a hidden
//! tab does), with its selection and scroll back.
//!
//! - **Matching** ([`Query`]): each space-separated word must match the name (AND). A word is a
//!   case-insensitive substring unless it has a capital (smart case: then capitals count), or a
//!   whole-name glob when it has `*`, `?` or `[` (`pattern.rs`), with the same smart case.
//! - **Results are entries** whose `os` is the path relative to the root (`src/main.rs`), so
//!   everything that acts on `cwd.join(os)` acts on the result's full path, never on "root +
//!   name". Keys are `r<n>`, unique in the process, so a row keeps its key when it's renamed.
//! - **The walk** ([`Pool`]): a few threads (at most [`THREADS`]) of the window's own, started by
//!   its first search and kept, share one queue of folders, breadth first. Each checks its
//!   walk's stop flag before every directory entry. Symlinked folders aren't entered (no loops),
//!   nor other file systems unless `search.cross_devices`, nor `/proc`, `/sys` and `/dev` on
//!   Linux, nor hidden folders while hidden files are hidden. Folders that can't be read are
//!   counted. At [`MAX_RESULTS`] the walk stops and says so.
//! - **Streaming**: workers add what they find to one shared batch and wake the window only
//!   when it has taken the last one, so the window takes at most one batch per frame and only
//!   appends it ([`Folder::poll_search`]). Progress without results wakes it at most every
//!   [`PROGRESS_EVERY`]. The list itself grows only to [`LIVE_ROWS`] while the walk goes on:
//!   Rune reads every row's key again whenever a list's row count changes (RUNE-GAPS.md), so
//!   a list growing every frame would cost the window time in proportion to the results. The
//!   rest join in one step when the walk ends or Esc stops it, as a folder that size would.
//!   The walking threads run below the window's priority (CPU and disk).
//! - **Cancelled** by a query change, Esc (keeping what was found), navigating away, and closing
//!   or hiding the tab: the walk's flag is set and its queued folders are dropped unread.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rune::{Key, KeyEvent, Latest, TextEdit, Tx};

use crate::app::State;
use crate::commands::Cmd;
use crate::folder::{Folder, TabMemo};
use crate::fs::{self, Entry};
use crate::pattern::{Case, Glob};

mod walk;
pub(crate) use walk::background;

pub use walk::*;

/// The most results a search keeps (each is a few hundred bytes); the walk stops there.
pub const MAX_RESULTS: usize = 100_000;

/// The most walking threads per window. Measured on a 4-core machine over a 550 MB tree of
/// 33,000 folders and 200,000 files (BENCH.md): 4 threads finish 2.3× faster than one.
pub const THREADS: usize = 4;

/// How often a walk that's finding nothing still wakes the window to update its count.
pub const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// The most rows the list shows while a walk is still going (the rest are counted, and join when
/// it ends). Plenty to look through, and a row count change of this size costs Rune about a
/// millisecond (it reads every key again).
pub const LIVE_ROWS: usize = 1_000;

/// Test only: holds a walk at the first entry of one folder, then counts what that walk looks
/// at after the gate opens, and says when it leaves the folder.
#[cfg(test)]
pub(crate) mod hook {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, Sender};

    pub(crate) struct Gate {
        pub dir: PathBuf,
        pub started: Sender<()>,
        pub go: Receiver<()>,
        /// Told the number of entries the walk looked at in `dir` after the gate opened.
        pub left: Sender<usize>,
    }

    struct Caught {
        walk: u64,
        dir: PathBuf,
        after: usize,
        left: Sender<usize>,
    }

    pub(crate) static GATES: Mutex<Vec<Gate>> = Mutex::new(Vec::new());
    /// Holds the first path a recheck looks at under `dir` until told, then counts the paths
    /// under `dir` that rechecks look at after that.
    pub(crate) struct RecheckGate {
        pub dir: PathBuf,
        pub started: Sender<()>,
        pub go: Option<Receiver<()>>,
        pub after: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    pub(crate) static RECHECK: Mutex<Vec<RecheckGate>> = Mutex::new(Vec::new());

    pub(super) fn recheck(path: &Path) {
        let go = {
            let mut g = RECHECK.lock().unwrap();
            let Some(gate) = g.iter_mut().find(|g| path.starts_with(&g.dir)) else { return };
            match gate.go.take() {
                Some(go) => {
                    let _ = gate.started.send(());
                    go
                }
                None => {
                    gate.after.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return;
                }
            }
        };
        let _ = go.recv_timeout(std::time::Duration::from_secs(5));
    }

    /// Folders that report another folder's identity, as a bind mount of it would.
    pub(crate) static ALIASES: Mutex<Vec<(PathBuf, PathBuf)>> = Mutex::new(Vec::new());

    pub(super) fn folder_alias(path: &Path) -> Option<crate::noclobber::FileId> {
        let of = ALIASES.lock().unwrap().iter().find(|(a, _)| a == path).map(|(_, of)| of.clone())?;
        crate::noclobber::dir_id(&of).map(|d| d.file_id())
    }

    /// Holds the device lookup for a root until told (or a few seconds pass).
    pub(crate) static DEVICES: Mutex<Vec<(PathBuf, Receiver<()>)>> = Mutex::new(Vec::new());

    pub(super) fn devices(root: &Path) {
        let gate = {
            let mut g = DEVICES.lock().unwrap();
            g.iter().position(|(d, _)| d == root).map(|i| g.remove(i))
        };
        if let Some((_, go)) = gate {
            let _ = go.recv_timeout(std::time::Duration::from_secs(3));
        }
    }
    static CAUGHT: Mutex<Vec<Caught>> = Mutex::new(Vec::new());

    pub(super) fn entry(dir: &Path, walk: u64) {
        let gate = {
            let mut g = GATES.lock().unwrap();
            g.iter().position(|g| g.dir == dir).map(|i| g.remove(i))
        };
        if let Some(gate) = gate {
            CAUGHT.lock().unwrap().push(Caught { walk, dir: dir.to_path_buf(), after: 0, left: gate.left });
            let _ = gate.started.send(());
            let _ = gate.go.recv();
            return;
        }
        if let Some(c) = CAUGHT.lock().unwrap().iter_mut().find(|c| c.walk == walk) {
            c.after += 1;
        }
    }

    pub(super) fn left(dir: &Path, walk: u64) {
        let mut c = CAUGHT.lock().unwrap();
        if let Some(i) = c.iter().position(|c| c.walk == walk && c.dir == dir) {
            let c = c.remove(i);
            let _ = c.left.send(c.after);
        }
    }
}

// ---- a search on screen -------------------------------------------------------------------

/// A pane's search view: the query field, the walk, and the listing to go back to.
pub struct Search {
    pub edit: TextEdit,
    /// The query the results are for (trimmed), and its parsed form.
    pub query: String,
    pub parsed: Option<Arc<Query>>,
    /// Why the query can't be used.
    pub bad: Option<String>,
    /// The plain listing's state, restored when the search ends.
    pub memo: TabMemo,
    pub walk: Option<Running>,
    /// Counts from the walk, as last taken.
    pub visited: u64,
    pub unreadable: u64,
    pub capped: bool,
    pub error: Option<String>,
    /// How long the last walk took, once it finished; `stopped` when Esc ended it early.
    pub took: Option<Duration>,
    pub stopped: bool,
    /// The user sorted the results (a header click): until then they're in the order found.
    pub sorted: bool,
    /// Which walk this is: a recheck asked for under an older one is dropped.
    pub generation: u64,
    /// How many of the results (`Folder::entries`, in order) the list has taken in: all of
    /// them once the walk is over, at most [`LIVE_ROWS`] while it goes on.
    pub upto: usize,
    /// Re-checks of results after a file operation, on a worker, and the stop flag of the one
    /// in flight: set when a newer one replaces it, the query changes, or the search ends.
    recheck: Latest<Recheck>,
    recheck_stop: Arc<AtomicBool>,
    /// What's waiting to be rechecked: everything since the last recheck that landed (a newer
    /// one replaces the one in flight, so it takes over its work too).
    pending: Pending,
    /// Rechecks asked for: only the latest one's answer clears `pending`.
    asked: u64,
    /// The root's canonical path, from the walk (see [`Folder::root_based`]).
    pub root_canon: Option<PathBuf>,
    /// What file operations moved or removed while the walk went on (`from`, and `to` when it
    /// stayed inside the root; paths below the root as this search spells them). A folder read
    /// before the change can deliver its results after it, under the old paths.
    changes: Vec<(PathBuf, Option<PathBuf>)>,
}

/// What a recheck has to look at.
#[derive(Default)]
struct Pending {
    /// Folders that changed: the results in them are looked at.
    folders: Vec<PathBuf>,
    /// Items that landed inside the root: they join if they match.
    landed: Vec<PathBuf>,
    /// Results (keys) that arrived after a change and may be under an old path.
    verify: HashSet<String>,
}

impl Search {
    /// Stops the walk and any recheck where they are, and forgets what was waiting for one.
    pub fn stop_all(&mut self) {
        self.walk = None;
        self.recheck_stop.store(true, Relaxed);
        self.pending = Pending::default();
        self.changes.clear();
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        self.recheck_stop.store(true, Relaxed);
    }
}

impl Search {
    pub fn new(memo: TabMemo, query: &str, waker: Option<rune::Waker>) -> Search {
        let mut edit = TextEdit::new(query);
        edit.focused = true;
        let recheck = Latest::default();
        if let Some(w) = waker {
            recheck.set_waker(w);
        }
        Search {
            edit,
            query: String::new(),
            parsed: None,
            bad: None,
            memo,
            walk: None,
            visited: 0,
            unreadable: 0,
            capped: false,
            error: None,
            took: None,
            stopped: false,
            sorted: false,
            generation: 0,
            upto: 0,
            recheck,
            recheck_stop: Arc::new(AtomicBool::new(false)),
            pending: Pending::default(),
            asked: 0,
            root_canon: None,
            changes: Vec::new(),
        }
    }

    pub fn walking(&self) -> bool {
        self.walk.is_some()
    }
}

/// What a recheck found: results gone, results changed, and new results (items an operation
/// put inside the root that match).
struct Recheck {
    generation: u64,
    /// Which recheck this answers.
    asked: u64,
    gone: HashSet<String>,
    /// Each with its identity now (what actions on it check from here on).
    changed: Vec<(String, bool, u64, i64, Option<fs::Ident>)>,
    added: Vec<Entry>,
    /// Results found under the path a change moved them to: their new path below the root.
    moved: Vec<(String, OsString)>,
}

/// One result a recheck looks at.
enum Look {
    /// The result itself: still there and still the item the walk found, or under `alt` (where
    /// a change moved it), or gone.
    Own { key: String, path: PathBuf, id: Option<fs::Ident>, alt: Option<PathBuf> },
    /// A result inside a changed folder's child: gone if that child is.
    Under { key: String, path: PathBuf },
}

/// "1,234".
pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl Folder {
    pub fn searching(&self) -> bool {
        self.search.is_some()
    }

    /// The results now, empty, for a new walk (or none): keys change, the list scrolls up.
    fn clear_results(&mut self) {
        if let Some(s) = self.search.as_mut() {
            s.upto = 0;
        }
        self.entries = Arc::default();
        self.visible = Arc::default();
        self.marked.clear();
        self.sel = 0;
        self.anchor = 0;
        self.drop_target = None;
        self.bump();
    }

    /// From `poll`: appends what the walk found since the last frame (one batch), and notes
    /// when it's done. True when anything on screen changed.
    pub fn poll_search(&mut self) -> bool {
        let Some(s) = self.search.as_mut() else { return false };
        let mut changed = false;
        if let Some(r) = s.recheck.take() {
            changed |= self.apply_recheck(r);
        }
        let Some(s) = self.search.as_mut() else { return changed };
        let Some(walk) = &s.walk else { return changed };
        if s.root_canon.is_none() {
            s.root_canon = walk.root_canon();
        }
        let d = walk.take();
        let moved = !d.batch.is_empty() || d.visited != s.visited || d.done || d.error != s.error;
        s.visited = d.visited;
        s.unreadable = d.unreadable;
        s.capped = d.capped;
        s.error = d.error;
        if d.done {
            s.took = Some(walk.elapsed());
            s.walk = None;
        }
        let resort = d.done && s.sorted;
        // A batch after a change may hold results read before it, under old paths: those under
        // a changed path are checked (by identity) before they can be trusted.
        let root = self.cwd.clone();
        let late: Vec<String> = d
            .batch
            .iter()
            .filter(|e| {
                let abs = root.join(&e.os);
                s.changes.iter().any(|(from, to)| abs.starts_with(from) || to.as_ref().is_some_and(|t| abs.starts_with(t)))
            })
            .map(|e| e.key.clone())
            .collect();
        let verify = !late.is_empty();
        s.pending.verify.extend(late);
        Arc::make_mut(&mut self.entries).extend(d.batch);
        self.take_in();
        if verify {
            self.request_recheck();
        }
        if resort {
            // Sorted by a header click while it was walking: what came after joins the order.
            self.resort();
        }
        changed | moved
    }

    /// Shows the results the list hasn't taken in yet: up to [`LIVE_ROWS`] while the walk goes
    /// on, then the rest. Appending only: rows keep their keys and places, so nothing moves
    /// under the pointer and virtual rows need no new version. True when the list grew.
    fn take_in(&mut self) -> bool {
        let Some(s) = self.search.as_mut() else { return false };
        let n = self.entries.len();
        let to = if s.walking() { n.min(LIVE_ROWS.max(s.upto)) } else { n };
        if to <= s.upto {
            return false;
        }
        let hidden = self.show_hidden;
        let from = std::mem::replace(&mut s.upto, to);
        Arc::make_mut(&mut self.visible).extend(&self.entries, from..to, |e| hidden || !e.hidden);
        true
    }

    /// Applies a recheck's findings, if it's for the walk on screen.
    fn apply_recheck(&mut self, mut r: Recheck) -> bool {
        let Some(s) = self.search.as_mut().filter(|s| s.generation == r.generation) else { return false };
        // The latest answer covers everything that was waiting; once the walk is over too, no
        // batch is still to come from before a change.
        if r.asked == s.asked {
            s.pending = Pending::default();
            if !s.walking() {
                s.changes.clear();
            }
        }
        let keep = self.cursor_name();
        // The selection rides on its entries while they're renamed, removed and added.
        self.carry_marks();
        // Results found where a change moved them: under their new path, unless a row for that
        // path is already there (read after the change), when this one goes.
        if !r.moved.is_empty() {
            let mut have: HashSet<OsString> = self.entries.iter().map(|e| e.os.clone()).collect();
            let moved: HashMap<String, OsString> = r.moved.drain(..).collect();
            for e in Arc::make_mut(&mut self.entries).iter_mut() {
                let Some(to) = moved.get(&e.key) else { continue };
                if !have.insert(to.clone()) {
                    r.gone.insert(e.key.clone());
                    continue;
                }
                e.os = to.clone();
                let name = Path::new(to).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                e.lower = name.to_lowercase();
                e.name = name;
            }
        }
        let changed: HashMap<&str, (bool, u64, i64, Option<fs::Ident>)> =
            r.changed.iter().map(|(k, d, s, m, id)| (k.as_str(), (*d, *s, *m, *id))).collect();
        let before = self.entries.len();
        let mut touched = !r.added.is_empty();
        let upto = self.search.as_ref().map_or(0, |s| s.upto);
        let (mut i, mut kept_shown, mut kept) = (0, 0, 0);
        // The carried selection moves down with the entries kept.
        let (old, mut carry) = (std::mem::take(&mut self.carry), crate::marked::Bits::default());
        Arc::make_mut(&mut self.entries).retain_mut(|e| {
            i += 1;
            if r.gone.contains(&e.key) {
                return false;
            }
            if old.get(i - 1) {
                carry.set(kept);
            }
            kept += 1;
            kept_shown += usize::from(i <= upto);
            if let Some(&(is_dir, size, mtime, id)) = changed.get(e.key.as_str()) {
                if (e.is_dir, e.size, e.mtime_ms) != (is_dir, size, mtime) {
                    (e.is_dir, e.size, e.mtime_ms) = (is_dir, size, mtime);
                    touched = true;
                }
                e.id = id;
            }
            true
        });
        self.carry = carry;
        touched |= self.entries.len() != before;
        if !touched {
            // Nothing moved: the rows are as they were.
            self.restore_marks();
            return false;
        }
        let names: HashSet<&OsStr> = self.entries.iter().map(|e| e.os.as_os_str()).collect();
        let added: Vec<Entry> = r.added.into_iter().filter(|e| !names.contains(e.os.as_os_str())).collect();
        Arc::make_mut(&mut self.entries).extend(added);
        if let Some(s) = self.search.as_mut() {
            // Added results join the list when it has taken in everything (else at the end).
            s.upto = if s.walking() { kept_shown } else { self.entries.len() };
        }
        self.refilter_carried(keep.as_deref());
        true
    }

    /// After `folders` changed on disk and items landed at `landed` (each spelled from this
    /// search's root, see [`Folder::root_based`]): re-checks, on a worker, the results the
    /// change may have touched (the root's whole walk isn't run again). A result directly in a
    /// changed folder is looked at itself, and must still be the item the walk found; deeper
    /// ones by their ancestor just below it (gone means they're gone too). Landed items inside
    /// the root that match the query join the results.
    pub fn recheck_results(&mut self, folders: &[PathBuf], landed: &[PathBuf]) {
        let Some(s) = self.search.as_mut() else { return };
        s.pending.folders.extend_from_slice(folders);
        s.pending.landed.extend_from_slice(landed);
        self.request_recheck();
    }

    /// Asks for a recheck of everything waiting (`Search::pending`), replacing (and stopping)
    /// the one in flight. The UI thread only picks the candidates.
    fn request_recheck(&mut self) {
        let root = self.cwd.clone();
        let Some(s) = self.search.as_ref() else { return };
        let Some(query) = s.parsed.clone() else { return };
        let generation = s.generation;
        let p = &s.pending;
        // Where a change moved `abs` to, if it did.
        let alt = |abs: &Path| s.changes.iter().rev().find_map(|(from, to)| Some(to.as_ref()?.join(abs.strip_prefix(from).ok()?)));
        let mut look: Vec<Look> = Vec::new();
        for e in self.entries.iter() {
            let abs = root.join(Path::new(&e.os));
            if p.verify.contains(&e.key) {
                look.push(Look::Own { key: e.key.clone(), alt: alt(&abs), path: abs, id: e.id });
                continue;
            }
            for f in &p.folders {
                let Ok(below) = abs.strip_prefix(f) else { continue };
                let mut parts = below.components();
                let first = parts.next();
                match (first, parts.next()) {
                    (Some(_), None) => look.push(Look::Own { key: e.key.clone(), alt: None, path: abs.clone(), id: e.id }),
                    (Some(c), Some(_)) => look.push(Look::Under { key: e.key.clone(), path: f.join(c) }),
                    _ => {}
                }
                break;
            }
        }
        let show_hidden = self.show_hidden;
        let new: Vec<PathBuf> = p
            .landed
            .iter()
            .filter(|p| p.strip_prefix(&root).is_ok_and(|r| !r.as_os_str().is_empty()))
            .filter(|p| p.file_name().is_some_and(|n| query.matches_name(&n.to_string_lossy())))
            .cloned()
            .collect();
        if look.is_empty() && new.is_empty() {
            return;
        }
        let Some(s) = self.search.as_mut() else { return };
        // A newer recheck replaces the one in flight: that one stops where it is.
        s.recheck_stop.store(true, Relaxed);
        let stop = Arc::new(AtomicBool::new(false));
        s.recheck_stop = stop.clone();
        s.asked += 1;
        let asked = s.asked;
        s.recheck.request_opt((look, new), move |(look, new)| {
            let mut out = Recheck { generation, asked, gone: HashSet::new(), changed: Vec::new(), added: Vec::new(), moved: Vec::new() };
            let mut seen: HashMap<PathBuf, bool> = HashMap::new();
            // What's at `path`, if it's the item `id` (any item, when not known): the same object
            // on the disk, edited or not (an edit shows as a change to its row).
            let found = |path: &Path, id: Option<fs::Ident>| path.symlink_metadata().ok().filter(|m| id.is_none() || fs::ident(m) == id);
            let info = |key: String, path: &Path, m: &std::fs::Metadata| {
                let target = if m.file_type().is_symlink() { std::fs::metadata(path).ok() } else { None };
                let t = target.as_ref().unwrap_or(m);
                let is_dir = t.is_dir();
                (key, is_dir, if is_dir { 0 } else { t.len() }, fs::mtime(t), fs::ident(m))
            };
            for l in look {
                if stop.load(Relaxed) {
                    return None;
                }
                match l {
                    Look::Own { key, path, id, alt } => {
                        #[cfg(test)]
                        hook::recheck(&path);
                        if stop.load(Relaxed) {
                            return None;
                        }
                        if let Some(m) = found(&path, id) {
                            out.changed.push(info(key, &path, &m));
                        } else if let Some(alt) = alt
                            && let Some(m) = found(&alt, id)
                            && let Ok(rel) = alt.strip_prefix(&root)
                        {
                            out.moved.push((key.clone(), rel.as_os_str().to_os_string()));
                            out.changed.push(info(key, &alt, &m));
                        } else {
                            out.gone.insert(key);
                        }
                    }
                    Look::Under { key, path } => {
                        #[cfg(test)]
                        hook::recheck(&path);
                        if !*seen.entry(path.clone()).or_insert_with(|| path.symlink_metadata().is_ok()) {
                            out.gone.insert(key);
                        }
                    }
                }
            }
            for p in new {
                if stop.load(Relaxed) {
                    return None;
                }
                if let Some(e) = entry_at(&root, &p, show_hidden) {
                    out.added.push(e);
                }
            }
            Some(out)
        });
    }

    /// `p` (absolute, from a file operation or another pane) as this search spells it: under
    /// its root as typed, or, through the canonical paths of the folders it's in (`canon`,
    /// from the operation) and of the root, under the same folders reached another way (a
    /// symlink to them). `None` when it's outside the root, or that can't be told.
    pub fn root_based(&self, p: &Path, canon: &[(PathBuf, PathBuf)]) -> Option<PathBuf> {
        if p.starts_with(&self.cwd) {
            return Some(p.to_path_buf());
        }
        let root = self.search.as_ref()?.root_canon.as_ref()?;
        let (given, c) = canon.iter().filter(|(g, _)| p.starts_with(g)).max_by_key(|(g, _)| g.as_os_str().len())?;
        let full = c.join(p.strip_prefix(given).ok()?);
        Some(self.cwd.join(full.strip_prefix(root).ok()?))
    }

    /// Items a file operation moved (`(from, to)`) or removed, with the canonical paths of
    /// the folders it worked in: the results among them show where they are now at once (a
    /// recheck follows), and a walk still going remembers them for the batches still to come.
    pub fn note_moves(&mut self, moved: &[(PathBuf, PathBuf)], removed: &[PathBuf], canon: &[(PathBuf, PathBuf)]) {
        if !self.searching() {
            return;
        }
        let mut pairs = Vec::new();
        let mut changes = Vec::new();
        for (from, to) in moved {
            let Some(from) = self.root_based(from, canon) else { continue };
            let to = self.root_based(to, canon);
            if let Some(to) = &to {
                pairs.push((from.clone(), to.clone()));
            }
            changes.push((from, to));
        }
        changes.extend(removed.iter().filter_map(|r| Some((self.root_based(r, canon)?, None))));
        if let Some(s) = self.search.as_mut()
            && s.walking()
        {
            s.changes.extend(changes);
        }
        self.renamed_paths(&pairs);
    }

    /// Each selected item's path and the identity its listing (or the search walk) read there,
    /// for an action to check first (`ops::Op::Checked`): an item replaced since is left alone,
    /// and one whose identity couldn't be read is never moved or removed.
    pub fn target_checks(&self) -> crate::ops::Checks {
        crate::ops::Checks::new(self.chosen(), self.targets().into_iter().map(|e| (self.cwd.join(&e.os), e.id)).collect())
    }

    /// [`Folder::target_checks`] for what a drag from row `i` carries.
    pub fn drag_checks(&self, i: usize) -> crate::ops::Checks {
        if self.is_selected(i) {
            return self.target_checks();
        }
        let items = self.visible.get(i).map(|&e| &self.entries[e as usize]).map(|e| (self.cwd.join(&e.os), e.id)).into_iter().collect();
        crate::ops::Checks::new(self.chosen(), items)
    }

    /// Where this folder's rows come from, for what a refusal says.
    pub fn chosen(&self) -> crate::ops::Chosen {
        if self.searching() { crate::ops::Chosen::Search } else { crate::ops::Chosen::Listing }
    }
}

/// A result for the item at `path` under `root`, as the walk would make it; `None` when it's
/// gone or hidden (while hidden files are).
fn entry_at(root: &Path, path: &Path, show_hidden: bool) -> Option<Entry> {
    let rel = path.strip_prefix(root).ok()?;
    if !show_hidden && rel.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')) {
        return None;
    }
    let m = path.symlink_metadata().ok()?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    let hidden = fs::is_hidden(&name, Some(&m));
    if hidden && !show_hidden {
        return None;
    }
    let is_link = m.file_type().is_symlink();
    let target = if is_link { std::fs::metadata(path).ok() } else { None };
    let t = target.as_ref().unwrap_or(&m);
    let is_dir = t.is_dir();
    Some(Entry {
        os: rel.as_os_str().to_os_string(),
        lower: name.to_lowercase(),
        name,
        key: next_key(),
        is_dir,
        is_link,
        hidden,
        size: if is_dir { 0 } else { t.len() },
        mtime_ms: fs::mtime(t),
        mode: fs::mode_of(&m),
        id: fs::ident_of(path, &m),
        pending: None,
    })
}

impl State {
    /// The window's walking threads, started on first use.
    fn search_pool(&mut self) -> &Pool {
        self.search_pool.get_or_insert_with(|| Pool::new(Pool::default_threads()))
    }

    /// ⌘F: the active tab becomes a search of its folder, the field focused. Already searching,
    /// it focuses the field again.
    pub fn open_search(&mut self, tx: &mut Tx) {
        self.close_overlays(tx);
        if self.rename.take().is_some() {
            tx.blur();
        }
        let ix = self.active;
        if let Some(s) = self.panes[ix].f.search.as_mut() {
            s.edit.focused = true;
            let n = s.edit.text.len();
            s.edit.set_selection(0, n);
            return;
        }
        let memo = self.panes[ix].f.memo();
        self.begin_search(ix, memo, "", tx);
    }

    /// Pane `ix` shows a search of its folder for `query`, keeping `memo` to go back to.
    pub(crate) fn begin_search(&mut self, ix: usize, memo: TabMemo, query: &str, tx: &mut Tx) {
        let waker = self.waker.clone();
        let f = &mut self.panes[ix].f;
        f.stop_read();
        f.pending = None;
        f.filter.clear();
        let mut search = Search::new(TabMemo { search: None, ..memo }, query, waker);
        // Only the active pane's field takes the keys.
        search.edit.focused = ix == self.active;
        f.search = Some(Box::new(search));
        f.clear_results();
        tx.scroll_to_instant(f.list_key(), 0.0, 0.0);
        self.restart_search(ix, tx);
    }

    /// The query field's binding (late writes after the search ends go to a scratch field).
    pub fn search_edit(&mut self, id: u64) -> &mut TextEdit {
        match self.panes.iter_mut().find(|p| p.id() == id).and_then(|p| p.f.search.as_mut()) {
            Some(s) => &mut s.edit,
            None => &mut self.palette_sink,
        }
    }

    /// The field's text changed: a new walk for the new query (the old one stops at once), or,
    /// cleared, back to the folder.
    pub fn search_changed(&mut self, id: u64, tx: &mut Tx) {
        tx.instant();
        let Some(ix) = self.pane_ix(id) else { return };
        let Some(s) = &self.panes[ix].f.search else { return };
        if s.edit.text.trim().is_empty() && !s.query.is_empty() {
            self.leave_search(ix, tx);
        } else if s.edit.text.trim() != s.query {
            self.restart_search(ix, tx);
        }
    }

    /// Starts a walk for what the field says, stopping the one before. Restarting on every
    /// keystroke (no debounce: that would need a timer) costs a few hundred microseconds here:
    /// the old walk's flag is set, the results cleared, one folder queued.
    pub(crate) fn restart_search(&mut self, ix: usize, tx: &mut Tx) {
        let (show_hidden, cross_devices) = (self.show_hidden, self.settings.search_cross_devices);
        let waker = self.waker.clone();
        let f = &mut self.panes[ix].f;
        let root = f.cwd.clone();
        let Some(s) = f.search.as_mut() else { return };
        s.stop_all();
        s.query = s.edit.text.trim().to_string();
        s.generation += 1;
        (s.visited, s.unreadable, s.capped, s.error, s.took, s.stopped, s.sorted) = (0, 0, false, None, None, false, false);
        let parsed = match Query::parse(&s.query) {
            Ok(q) => {
                s.bad = None;
                q.map(Arc::new)
            }
            Err(e) => {
                s.bad = Some(e);
                None
            }
        };
        s.parsed = parsed.clone();
        f.now = crate::folder::now();
        f.clear_results();
        tx.scroll_to_instant(f.list_key(), 0.0, 0.0);
        if let Some(query) = parsed {
            let spec = Spec { root, query, show_hidden, cross_devices, max: MAX_RESULTS };
            let running = self.search_pool().start(spec, waker);
            if let Some(s) = self.panes[ix].f.search.as_mut() {
                s.walk = Some(running);
            }
        }
    }

    /// Esc while walking: stops, keeping what was found.
    pub(crate) fn stop_search(&mut self, ix: usize) {
        let f = &mut self.panes[ix].f;
        // What's waiting in the batch is found too.
        f.poll_search();
        if let Some(s) = f.search.as_mut()
            && let Some(w) = s.walk.take()
        {
            s.took = Some(w.elapsed());
            s.stopped = true;
            // What it found and the list hadn't taken in yet joins it.
            let sorted = s.sorted;
            f.take_in();
            if sorted {
                f.resort();
            }
        }
    }

    /// Back to the plain folder: read fresh (waiting at most a frame, as showing a tab does),
    /// with the selection and scroll it had.
    pub(crate) fn leave_search(&mut self, ix: usize, tx: &mut Tx) {
        let Some(s) = self.panes[ix].f.search.take() else { return };
        let focused = s.edit.focused;
        // The view it has now, which may have changed during the search.
        let memo = TabMemo { view: self.panes[ix].f.view, ..s.memo.clone() };
        drop(s);
        if focused && ix == self.active {
            tx.blur();
        }
        let wait = self.show_wait;
        let f = &mut self.panes[ix].f;
        f.show(memo);
        if let Some(a) = f.request_now(wait) {
            self.land(ix, a, tx);
        }
        self.sync_preview();
    }

    /// Esc in search: stops a walk, else leaves.
    pub(crate) fn search_escape(&mut self, ix: usize, tx: &mut Tx) {
        if self.panes[ix].f.search.as_ref().is_some_and(|s| s.walking()) {
            self.stop_search(ix);
        } else {
            self.leave_search(ix, tx);
        }
    }

    /// Keys in the query field, before it edits: ↑↓ and Page Up/Down move through the results
    /// (typing stays in the field), Enter and Tab go to the results (so Space, Delete and the
    /// rest act on them), Esc stops or leaves, and command keys other than text editing's run.
    pub fn search_field_key(&mut self, id: u64, ev: &KeyEvent, tx: &mut Tx) -> bool {
        tx.instant();
        let Some(ix) = self.pane_ix(id) else { return false };
        self.activate(ix, tx);
        let m = ev.mods;
        let plain = !m.cmd && !m.ctrl && !m.alt;
        let page = 20;
        let handled = match &ev.key {
            Key::Escape if plain => {
                self.search_escape(ix, tx);
                true
            }
            Key::Up if plain && !m.shift => self.step_results(ix, -1, tx),
            Key::Down if plain && !m.shift => self.step_results(ix, 1, tx),
            Key::PageUp if plain && !m.shift => self.step_results(ix, -page, tx),
            Key::PageDown if plain && !m.shift => self.step_results(ix, page, tx),
            Key::Enter | Key::Tab if plain && !m.shift => {
                self.focus_results(ix, tx);
                true
            }
            // Typing, deleting and moving the caret are the field's.
            Key::Text(_) if !m.cmd && !m.ctrl => false,
            Key::Backspace | Key::Delete | Key::Left | Key::Right | Key::Home | Key::End => false,
            _ => match self.keymap.find(|c| c.matches_tx(ev, tx)) {
                // The field's own copy, cut, paste, select-all and undo.
                Some(Cmd::Copy | Cmd::Cut | Cmd::Paste | Cmd::PasteMove | Cmd::SelectAll | Cmd::Undo | Cmd::Redo) | None => false,
                Some(cmd) => {
                    if self.unavailable(cmd).is_none() {
                        self.run(cmd, tx);
                    }
                    true
                }
            },
        };
        if handled {
            self.sync_preview();
        }
        handled
    }

    fn step_results(&mut self, ix: usize, by: isize, tx: &mut Tx) -> bool {
        self.panes[ix].f.step(by, false, tx);
        true
    }

    /// Keys go to the results, out of the field.
    pub(crate) fn focus_results(&mut self, ix: usize, tx: &mut Tx) {
        if let Some(s) = self.panes[ix].f.search.as_mut()
            && s.edit.focused
        {
            s.edit.focused = false;
            tx.blur();
        }
    }

    /// Typing outside the field (the results have the keys): it goes into the query, which
    /// takes the keys back. `None` for backspace.
    pub(crate) fn type_in_search(&mut self, ix: usize, text: Option<&str>, tx: &mut Tx) {
        let Some(s) = self.panes[ix].f.search.as_mut() else { return };
        let n = s.edit.text.len();
        s.edit.set_selection(n, n);
        match text {
            Some(t) => s.edit.insert(t),
            None => {
                s.edit.text.pop();
            }
        }
        let n = s.edit.text.len();
        s.edit.set_selection(n, n);
        s.edit.focused = true;
        let id = self.panes[ix].id();
        self.search_changed(id, tx);
    }

    /// From `poll`: every searching pane takes its batch.
    pub(crate) fn poll_search(&mut self) -> bool {
        let mut changed = false;
        for p in &mut self.panes {
            changed |= p.f.poll_search();
        }
        changed
    }

    /// The status bar's line for a search: "Searching… 1,234 found in 18,000 items", then
    /// "1,234 found in 52,113 items · 1.8 s".
    pub fn search_status(f: &Folder) -> Option<String> {
        let s = f.search.as_ref()?;
        if let Some(e) = &s.error {
            return Some(format!("Couldn't search {}: {e}", fs::pretty(&f.cwd)));
        }
        if let Some(why) = &s.bad {
            return Some(why.clone());
        }
        if s.query.is_empty() {
            return Some(format!("Search names in {} and every folder in it", fs::pretty(&f.cwd)));
        }
        let found = count(f.entries.len() as u64);
        let seen = count(s.visited);
        let mut line = match s.took {
            None if f.entries.len() > s.upto => {
                format!("Searching… {found} found in {seen} items · showing the first {} until it's done", count(s.upto as u64))
            }
            None => format!("Searching… {found} found in {seen} items"),
            Some(t) => {
                let secs = t.as_secs_f64();
                let took = if secs < 1.0 {
                    format!("{} ms", t.as_millis())
                } else if secs < 10.0 {
                    format!("{secs:.1} s")
                } else {
                    format!("{secs:.0} s")
                };
                let stopped = if s.stopped { "Stopped · " } else { "" };
                format!("{stopped}{found} found in {seen} items · {took}")
            }
        };
        if s.capped {
            line.push_str(&format!(" · stopped at {} results", count(MAX_RESULTS as u64)));
        }
        if s.unreadable > 0 {
            let n = s.unreadable;
            line.push_str(&format!(" · {} folder{} couldn't be read", count(n), if n == 1 { "" } else { "s" }));
        }
        Some(line)
    }
}

#[cfg(test)]
pub(crate) mod tests;
