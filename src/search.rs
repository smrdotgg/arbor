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

// ---- matching -----------------------------------------------------------------------------

enum Word {
    /// A substring; `fold`: lowercased, matched against the lowercased name.
    Sub {
        text: String,
        fold: bool,
    },
    Glob(Glob),
}

/// What's typed in the search field, parsed: every word must match a name.
pub struct Query {
    words: Vec<Word>,
    /// Some word ignores case, so names need lowercasing.
    folds: bool,
}

impl Query {
    /// `None` for an empty query; an error for a glob that can't be used.
    pub fn parse(q: &str) -> Result<Option<Query>, String> {
        let mut words = Vec::new();
        for w in q.split_whitespace() {
            let fold = !w.chars().any(char::is_uppercase);
            if w.contains(['*', '?', '[']) {
                words.push(Word::Glob(Glob::name(w, if fold { Case::Insensitive } else { Case::Sensitive })?));
            } else {
                words.push(Word::Sub { text: if fold { w.to_lowercase() } else { w.to_string() }, fold });
            }
        }
        let folds = words.iter().any(|w| match w {
            Word::Sub { fold, .. } => *fold,
            Word::Glob(g) => g.case() == Case::Insensitive,
        });
        Ok((!words.is_empty()).then_some(Query { words, folds }))
    }

    /// Whether `name` matches; `lower` is `name.to_lowercase()` (only read when [`folds`]).
    pub fn matches(&self, name: &str, lower: &str) -> bool {
        self.words.iter().all(|w| match w {
            Word::Sub { text, fold: true } => lower.contains(text.as_str()),
            Word::Sub { text, fold: false } => name.contains(text.as_str()),
            Word::Glob(g) => g.matches_folded(if g.case() == Case::Insensitive { lower } else { name }),
        })
    }

    /// [`matches`](Self::matches) for a bare name.
    pub fn matches_name(&self, name: &str) -> bool {
        self.matches(name, &if self.folds { name.to_lowercase() } else { String::new() })
    }
}

/// `name` lowercased into `buf` (no allocation for an ASCII name once `buf` has grown).
fn fold<'a>(name: &str, buf: &'a mut String) -> &'a str {
    buf.clear();
    if name.is_ascii() {
        buf.extend(name.bytes().map(|b| b.to_ascii_lowercase() as char));
    } else {
        buf.push_str(&name.to_lowercase());
    }
    buf
}

// ---- the walk -----------------------------------------------------------------------------

/// How a walk goes: where, for what, and what it skips.
pub struct Spec {
    pub root: PathBuf,
    pub query: Arc<Query>,
    pub show_hidden: bool,
    pub cross_devices: bool,
    pub max: usize,
}

/// What a walk has found since the window last looked, and how far it has got.
#[derive(Default)]
pub struct Drained {
    pub batch: Vec<Entry>,
    pub visited: u64,
    pub found: usize,
    pub unreadable: u64,
    pub capped: bool,
    pub done: bool,
    /// The root itself couldn't be read.
    pub error: Option<String>,
}

#[derive(Default)]
struct Out {
    d: Drained,
    /// The window has been woken and hasn't taken the batch yet.
    woken: bool,
    /// When it was last woken, from the walk's start.
    woke_at: Duration,
}

/// One walk, shared by the threads working on it.
struct Walk {
    spec: Spec,
    /// The devices it stays on (unless `cross_devices`): found by the thread that reads the
    /// root, before it queues anything, so starting a walk touches no disk on the window's
    /// thread (a slow or sleeping disk would hold up the key that started it).
    devices: std::sync::OnceLock<Vec<u64>>,
    /// Folders never entered: pseudo file systems, and on macOS the data volume's second path.
    skip: Vec<PathBuf>,
    stop: AtomicBool,
    /// Folders queued or being read; the walk is done when it reaches 0.
    pending: AtomicUsize,
    out: Mutex<Out>,
    waker: Option<rune::Waker>,
    started: Instant,
    /// Test only: which walk this is.
    #[cfg(test)]
    serial: u64,
}

struct Task {
    walk: Arc<Walk>,
    dir: PathBuf,
    /// `dir` relative to the root.
    rel: PathBuf,
}

#[derive(Default)]
struct Queue {
    tasks: VecDeque<Task>,
    closed: bool,
}

#[derive(Default)]
struct Shared {
    q: Mutex<Queue>,
    cv: Condvar,
}

/// The window's walking threads, started by its first search and kept (idle, they wait on a
/// condition variable: no polling). Dropping it ends them once they finish what they're reading.
pub struct Pool {
    shared: Arc<Shared>,
}

impl Pool {
    pub fn new(threads: usize) -> Pool {
        let shared = Arc::new(Shared::default());
        for _ in 0..threads.max(1) {
            let s = shared.clone();
            std::thread::spawn(move || work(&s));
        }
        Pool { shared }
    }

    /// [`THREADS`], or fewer on a machine with fewer cores.
    pub fn default_threads() -> usize {
        std::thread::available_parallelism().map_or(2, |n| n.get()).min(THREADS)
    }

    /// Starts walking `spec.root`. The walk stops when the returned handle drops.
    pub fn start(&self, spec: Spec, waker: Option<rune::Waker>) -> Running {
        let mut skip: Vec<PathBuf> = Vec::new();
        if cfg!(target_os = "linux") {
            skip.extend(["/proc", "/sys", "/dev"].map(PathBuf::from));
        }
        if cfg!(target_os = "macos") {
            // The data volume, firmlinked into / already (/Users, /Applications…).
            skip.push(PathBuf::from("/System/Volumes/Data"));
        }
        skip.retain(|p| !spec.root.starts_with(p));
        let walk = Arc::new(Walk {
            spec,
            devices: std::sync::OnceLock::new(),
            skip,
            stop: AtomicBool::new(false),
            pending: AtomicUsize::new(1),
            out: Mutex::new(Out::default()),
            waker,
            started: Instant::now(),
            #[cfg(test)]
            serial: next_key().trim_start_matches('r').parse().unwrap_or(0),
        });
        let task = Task { walk: walk.clone(), dir: walk.spec.root.clone(), rel: PathBuf::new() };
        if let Ok(mut q) = self.shared.q.lock() {
            q.tasks.push_back(task);
        }
        self.shared.cv.notify_one();
        Running { walk }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        if let Ok(mut q) = self.shared.q.lock() {
            q.closed = true;
            q.tasks.clear();
        }
        self.shared.cv.notify_all();
    }
}

/// A walk in progress, from the window's side. Dropping it stops the walk.
pub struct Running {
    walk: Arc<Walk>,
}

impl Running {
    /// Takes what was found since the last call (one batch), and lets the walk wake the window
    /// again.
    pub fn take(&self) -> Drained {
        let mut out = self.walk.out.lock().unwrap_or_else(|e| e.into_inner());
        out.woken = false;
        let d = &mut out.d;
        Drained {
            batch: std::mem::take(&mut d.batch),
            visited: d.visited,
            found: d.found,
            unreadable: d.unreadable,
            capped: d.capped,
            done: d.done,
            error: d.error.clone(),
        }
    }

    pub fn stop(&self) {
        self.walk.stop.store(true, Relaxed);
    }

    /// How long it has been going.
    pub fn elapsed(&self) -> Duration {
        self.walk.started.elapsed()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The devices a walk from `root` stays on: the root's, and on macOS both halves of the startup
/// disk (the sealed system volume and its data volume, joined by firmlinks).
fn devices_for(root: &Path) -> Vec<u64> {
    #[cfg(test)]
    hook::devices(root);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = |p: &Path| std::fs::metadata(p).ok().map(|m| m.dev());
        let mut v: Vec<u64> = dev(root).into_iter().collect();
        if cfg!(target_os = "macos") {
            let pair = [dev(Path::new("/")), dev(Path::new("/System/Volumes/Data"))];
            if pair.iter().flatten().any(|d| v.contains(d)) {
                v.extend(pair.into_iter().flatten());
            }
        }
        v
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        Vec::new()
    }
}

/// A walking thread: takes folders off the queue until the pool closes.
fn work(s: &Shared) {
    background();
    let mut buf = String::new();
    loop {
        let task = {
            let mut q = s.q.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if q.closed {
                    return;
                }
                if let Some(t) = q.tasks.pop_front() {
                    break t;
                }
                q = s.cv.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        // A stopped walk's folders are dropped unread.
        if !task.walk.stop.load(Relaxed) {
            read_dir(s, &task, &mut buf);
        }
        let walk = task.walk;
        if walk.pending.fetch_sub(1, Relaxed) == 1 {
            walk.deliver(Vec::new(), 0, true);
        }
    }
}

/// Puts the calling thread below the window's priority: CPU, and the disk where the platform
/// has a per-thread setting. A big walk on a small machine then slows its own results, not the
/// window (or other apps).
fn background() {
    #[cfg(target_os = "linux")]
    // SAFETY: plain syscalls on the calling thread, with constant arguments.
    unsafe {
        // Linux applies both per thread: nice 10, and the lowest best-effort disk priority.
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 10);
        const IOPRIO_WHO_PROCESS: libc::c_long = 1;
        const IOPRIO_BEST_EFFORT_LOWEST: libc::c_long = (2 << 13) | 7;
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, IOPRIO_BEST_EFFORT_LOWEST);
    }
    #[cfg(target_os = "macos")]
    // SAFETY: sets the calling thread's own QoS class.
    unsafe {
        // Utility: user-visible progress, below the window's interactive class for CPU and I/O.
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
    }
    #[cfg(windows)]
    // SAFETY: the pseudo handle of the calling thread, and a valid priority.
    unsafe {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

/// Reads one folder: queues its subfolders, and hands over its matches.
fn read_dir(s: &Shared, t: &Task, buf: &mut String) {
    let walk = &t.walk;
    let spec = &walk.spec;
    // The root's task comes first, and nothing else is queued until it has read the root.
    let devices = walk.devices.get_or_init(|| devices_for(&spec.root));
    let rd = match std::fs::read_dir(&t.dir) {
        Ok(rd) => rd,
        Err(e) => {
            let mut out = walk.out.lock().unwrap_or_else(|e| e.into_inner());
            if t.rel.as_os_str().is_empty() {
                out.d.error = Some(e.to_string());
            } else {
                out.d.unreadable += 1;
            }
            return;
        }
    };
    let (mut found, mut subdirs, mut seen) = (Vec::new(), Vec::new(), 0u64);
    for de in rd {
        #[cfg(test)]
        hook::entry(&t.dir, walk.serial);
        if walk.stop.load(Relaxed) {
            break;
        }
        let Ok(de) = de else { continue };
        seen += 1;
        let os = de.file_name();
        let name = os.to_string_lossy();
        if name.starts_with('.') && !spec.show_hidden {
            continue;
        }
        // From the directory entry itself on every platform: no stat.
        let ft = de.file_type().ok();
        let is_link = ft.is_some_and(|t| t.is_symlink());
        // Not a link: a link to a folder is never entered (no loops). On Windows junctions and
        // mounted volumes are links too.
        let real_dir = ft.is_some_and(|t| t.is_dir());
        // Windows has it from the directory listing; elsewhere one lstat, only when needed.
        let mut meta = if cfg!(windows) { de.metadata().ok() } else { None };
        if !spec.show_hidden && cfg!(windows) && fs::is_hidden(&name, meta.as_ref()) {
            continue;
        }
        if real_dir {
            let path = de.path();
            if !walk.skip.contains(&path) {
                let look = cfg!(target_os = "macos") || (cfg!(unix) && !spec.cross_devices);
                if look && meta.is_none() {
                    meta = de.metadata().ok();
                }
                // macOS marks some folders hidden by a flag (~/Library).
                if !spec.show_hidden && cfg!(target_os = "macos") && fs::is_hidden(&name, meta.as_ref()) {
                    continue;
                }
                if spec.cross_devices || on_devices(meta.as_ref(), devices) {
                    subdirs.push(Task { walk: walk.clone(), dir: path, rel: t.rel.join(&os) });
                }
            }
        }
        let lower = if walk.spec.query.folds { fold(&name, buf) } else { "" };
        if !spec.query.matches(&name, lower) {
            continue;
        }
        if meta.is_none() {
            meta = de.metadata().ok();
        }
        if !spec.show_hidden && cfg!(target_os = "macos") && fs::is_hidden(&name, meta.as_ref()) {
            continue;
        }
        let mut is_dir = real_dir;
        let (mut size, mut mtime_ms) = meta.as_ref().map_or((0, 0), |m| (m.len(), fs::mtime(m)));
        if is_link && let Ok(target) = std::fs::metadata(de.path()) {
            // Followed once, as a listing does, so a link to a folder shows as a folder.
            is_dir = target.is_dir();
            size = target.len();
            mtime_ms = fs::mtime(&target);
        }
        let hidden = fs::is_hidden(&name, meta.as_ref());
        let lower = if walk.spec.query.folds { lower.to_string() } else { name.to_lowercase() };
        found.push(Entry {
            name: name.into_owned(),
            lower,
            key: next_key(),
            is_dir,
            is_link,
            hidden,
            size: if is_dir { 0 } else { size },
            mtime_ms,
            os: t.rel.join(&os).into_os_string(),
        });
    }
    if !subdirs.is_empty() && !walk.stop.load(Relaxed) {
        walk.pending.fetch_add(subdirs.len(), Relaxed);
        let n = subdirs.len();
        if let Ok(mut q) = s.q.lock() {
            q.tasks.extend(subdirs);
        }
        if n == 1 {
            s.cv.notify_one();
        } else {
            s.cv.notify_all();
        }
    }
    #[cfg(test)]
    hook::left(&t.dir, walk.serial);
    walk.deliver(found, seen, false);
}

/// Whether a folder with metadata `m` is on one of `devices` (unknown counts as on them).
fn on_devices(m: Option<&std::fs::Metadata>, devices: &[u64]) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        m.is_none_or(|m| devices.is_empty() || devices.contains(&m.dev()))
    }
    #[cfg(not(unix))]
    {
        let _ = (m, devices);
        true
    }
}

impl Walk {
    /// Adds one folder's matches and count to the shared batch, and wakes the window if it has
    /// taken the last batch (and there's something to show, or progress is due).
    fn deliver(&self, mut found: Vec<Entry>, seen: u64, done: bool) {
        let wake = {
            let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
            let d = &mut out.d;
            if !d.capped {
                let room = self.spec.max.saturating_sub(d.found);
                if found.len() >= room && !found.is_empty() {
                    found.truncate(room);
                    d.capped = true;
                    self.stop.store(true, Relaxed);
                }
                d.found += found.len();
                d.batch.append(&mut found);
            }
            d.visited += seen;
            d.done |= done;
            let now = self.started.elapsed();
            let due = !d.batch.is_empty() || d.done || now.saturating_sub(out.woke_at) >= PROGRESS_EVERY;
            let wake = !out.woken && due;
            if wake {
                out.woken = true;
                out.woke_at = now;
            }
            wake
        };
        if wake && let Some(w) = &self.waker {
            w();
        }
    }
}

/// A fresh row key: `r` and a number never used before in this process.
fn next_key() -> String {
    static KEY: AtomicU64 = AtomicU64::new(1);
    format!("r{}", KEY.fetch_add(1, Relaxed))
}

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
}

impl Search {
    /// Stops the walk and any recheck where they are.
    pub fn stop_all(&mut self) {
        self.walk = None;
        self.recheck_stop.store(true, Relaxed);
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
    gone: HashSet<String>,
    changed: Vec<(String, bool, u64, i64)>,
    added: Vec<Entry>,
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
        self.entries.clear();
        self.visible.clear();
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
        self.entries.extend(d.batch);
        self.take_in();
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
        self.visible.extend((from..to).filter(|&i| hidden || !self.entries[i].hidden).map(|i| i as u32));
        true
    }

    /// Applies a recheck's findings, if it's for the walk on screen.
    fn apply_recheck(&mut self, r: Recheck) -> bool {
        if self.search.as_ref().is_none_or(|s| s.generation != r.generation) {
            return false;
        }
        let keep = self.cursor_name();
        let changed: HashMap<&str, (bool, u64, i64)> = r.changed.iter().map(|(k, d, s, m)| (k.as_str(), (*d, *s, *m))).collect();
        let before = self.entries.len();
        let mut touched = !r.added.is_empty();
        let upto = self.search.as_ref().map_or(0, |s| s.upto);
        let (mut i, mut kept_shown) = (0, 0);
        self.entries.retain_mut(|e| {
            i += 1;
            if r.gone.contains(&e.key) {
                return false;
            }
            kept_shown += usize::from(i <= upto);
            if let Some(&(is_dir, size, mtime)) = changed.get(e.key.as_str())
                && (e.is_dir, e.size, e.mtime_ms) != (is_dir, size, mtime)
            {
                (e.is_dir, e.size, e.mtime_ms) = (is_dir, size, mtime);
                touched = true;
            }
            true
        });
        touched |= self.entries.len() != before;
        if !touched {
            return false;
        }
        let names: HashSet<&OsStr> = self.entries.iter().map(|e| e.os.as_os_str()).collect();
        let added: Vec<Entry> = r.added.into_iter().filter(|e| !names.contains(e.os.as_os_str())).collect();
        self.entries.extend(added);
        let names: HashSet<OsString> = self.entries.iter().map(|e| e.os.clone()).collect();
        self.marked.retain(|n| names.contains(n));
        if let Some(s) = self.search.as_mut() {
            // Added results join the list when it has taken in everything (else at the end).
            s.upto = if s.walking() { kept_shown } else { self.entries.len() };
        }
        self.refilter(keep.as_deref());
        true
    }

    /// After `folders` changed on disk (each absolute) and items landed at `landed`: re-checks,
    /// on a worker, the results the change may have touched (the root's whole walk isn't run
    /// again). A result directly in a changed folder is looked at itself; deeper ones by their
    /// ancestor just below it (gone means they're gone too). Landed items inside the root that
    /// match the query join the results. The UI thread only picks the candidates.
    pub fn recheck_results(&mut self, folders: &[PathBuf], landed: &[PathBuf]) {
        let root = self.cwd.clone();
        let Some(s) = self.search.as_ref() else { return };
        let Some(query) = s.parsed.clone() else { return };
        let generation = s.generation;
        // (key, absolute path to look at, whether that's the result itself)
        let mut look: Vec<(String, PathBuf, bool)> = Vec::new();
        for e in &self.entries {
            let rel = Path::new(&e.os);
            let abs = root.join(rel);
            for f in folders {
                let Ok(below) = abs.strip_prefix(f) else { continue };
                let mut parts = below.components();
                let first = parts.next();
                match (first, parts.next()) {
                    (Some(_), None) => look.push((e.key.clone(), abs.clone(), true)),
                    (Some(c), Some(_)) => look.push((e.key.clone(), f.join(c), false)),
                    _ => {}
                }
                break;
            }
        }
        let show_hidden = self.show_hidden;
        let new: Vec<PathBuf> = landed
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
        s.recheck.request_opt((look, new), move |(look, new)| {
            let mut out = Recheck { generation, gone: HashSet::new(), changed: Vec::new(), added: Vec::new() };
            let mut seen: HashMap<PathBuf, bool> = HashMap::new();
            for (key, path, own) in look {
                #[cfg(test)]
                hook::recheck(&path);
                if stop.load(Relaxed) {
                    return None;
                }
                if own {
                    match path.symlink_metadata() {
                        Ok(m) => {
                            let target = if m.file_type().is_symlink() { std::fs::metadata(&path).ok() } else { None };
                            let t = target.as_ref().unwrap_or(&m);
                            let is_dir = t.is_dir();
                            out.changed.push((key, is_dir, if is_dir { 0 } else { t.len() }, fs::mtime(t)));
                        }
                        Err(_) => {
                            out.gone.insert(key);
                        }
                    }
                } else if !*seen.entry(path.clone()).or_insert_with(|| path.symlink_metadata().is_ok()) {
                    out.gone.insert(key);
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

    /// The result at `from` was renamed to `to` (both absolute): it shows under its new name at
    /// once, keeping its row, and so do the results inside it when it's a folder, so no row
    /// names a path that's gone (or now another item).
    pub fn renamed_result(&mut self, from: &Path, to: &Path) {
        let (Ok(old), Ok(new)) = (from.strip_prefix(&self.cwd), to.strip_prefix(&self.cwd)) else { return };
        let (old, new) = (old.to_path_buf(), new.to_path_buf());
        let mut marked = std::mem::take(&mut self.marked);
        for e in &mut self.entries {
            let rel = Path::new(&e.os);
            let moved = if rel == old {
                let name = new.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                e.hidden = name.starts_with('.') || (e.hidden && !e.name.starts_with('.'));
                e.lower = name.to_lowercase();
                e.name = name;
                new.clone()
            } else if let Ok(below) = rel.strip_prefix(&old) {
                new.join(below)
            } else {
                continue;
            };
            let was = std::mem::replace(&mut e.os, moved.into_os_string());
            if marked.remove(&was) {
                marked.insert(e.os.clone());
            }
        }
        self.marked = marked;
        let keep = self.cursor_name();
        self.refilter(keep.as_deref());
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
        let memo = s.memo.clone();
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
                Some(Cmd::Copy | Cmd::Cut | Cmd::Paste | Cmd::PasteMove | Cmd::SelectAll | Cmd::Undo) | None => false,
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
#[path = "search_tests.rs"]
mod tests;
