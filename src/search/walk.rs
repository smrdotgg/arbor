//! The walk: matching names against the query, and the pool of walker threads (at most
//! [`THREADS`], below the window's priority) sharing one queue of folders, a stop flag per
//! walk, and one batch of results the window takes at most once a frame.

use super::*;

// ---- matching -----------------------------------------------------------------------------

pub(super) enum Word {
    /// A substring; `fold`: lowercased, matched against the lowercased name.
    Sub {
        text: String,
        fold: bool,
    },
    Glob(Glob),
}

/// What's typed in the search field, parsed: every word must match a name.
pub struct Query {
    pub(super) words: Vec<Word>,
    /// Some word ignores case, so names need lowercasing.
    pub(super) folds: bool,
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
pub(super) fn fold<'a>(name: &str, buf: &'a mut String) -> &'a str {
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
pub(super) struct Out {
    pub(super) d: Drained,
    /// The window has been woken and hasn't taken the batch yet.
    pub(super) woken: bool,
    /// When it was last woken, from the walk's start.
    pub(super) woke_at: Duration,
}

/// One walk, shared by the threads working on it.
pub(super) struct Walk {
    pub(super) spec: Spec,
    /// The devices it stays on (unless `cross_devices`): found by the thread that reads the
    /// root, before it queues anything, so starting a walk touches no disk on the window's
    /// thread (a slow or sleeping disk would hold up the key that started it).
    pub(super) devices: std::sync::OnceLock<Vec<u64>>,
    /// The folders it has entered, by identity (device and inode), so one reached by two paths
    /// (a bind mount of a folder on the same disk, or one of its ancestors) is walked once.
    pub(super) entered: Mutex<HashSet<crate::noclobber::FileId>>,
    /// The root's canonical path (links resolved), found with its devices: a change made
    /// through another path to the same folders (a symlink) is matched by it.
    pub(super) root_canon: std::sync::OnceLock<Option<PathBuf>>,
    /// Folders never entered: pseudo file systems, and on macOS the data volume's second path.
    pub(super) skip: Vec<PathBuf>,
    pub(super) stop: AtomicBool,
    /// Folders queued or being read; the walk is done when it reaches 0.
    pub(super) pending: AtomicUsize,
    pub(super) out: Mutex<Out>,
    pub(super) waker: Option<rune::Waker>,
    pub(super) started: Instant,
    /// Test only: which walk this is.
    #[cfg(test)]
    pub(super) serial: u64,
}

pub(super) struct Task {
    pub(super) walk: Arc<Walk>,
    pub(super) dir: PathBuf,
    /// `dir` relative to the root.
    pub(super) rel: PathBuf,
}

#[derive(Default)]
pub(super) struct Queue {
    pub(super) tasks: VecDeque<Task>,
    pub(super) closed: bool,
}

#[derive(Default)]
pub(super) struct Shared {
    pub(super) q: Mutex<Queue>,
    pub(super) cv: Condvar,
}

/// The window's walking threads, started by its first search and kept (idle, they wait on a
/// condition variable: no polling). Dropping it ends them once they finish what they're reading.
pub struct Pool {
    pub(super) shared: Arc<Shared>,
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
            entered: Mutex::new(HashSet::new()),
            root_canon: std::sync::OnceLock::new(),
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
    pub(super) walk: Arc<Walk>,
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

    /// The root's canonical path, once the walk has looked it up.
    pub fn root_canon(&self) -> Option<PathBuf> {
        self.walk.root_canon.get().cloned().flatten()
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
pub(super) fn devices_for(root: &Path) -> Vec<u64> {
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
pub(super) fn work(s: &Shared) {
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
pub(crate) fn background() {
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
pub(super) fn read_dir(s: &Shared, t: &Task, buf: &mut String) {
    let walk = &t.walk;
    let spec = &walk.spec;
    // The root's task comes first, and nothing else is queued until it has read the root.
    let devices = walk.devices.get_or_init(|| {
        let _ = walk.root_canon.set(std::fs::canonicalize(&spec.root).ok());
        if let Some(id) = std::fs::metadata(&spec.root).ok().and_then(|m| folder_id(&spec.root, &m)) {
            walk.entered.lock().unwrap_or_else(|e| e.into_inner()).insert(id);
        }
        devices_for(&spec.root)
    });
    // Windows: from the enumeration that gives each item's volume and file id, so a result's
    // identity is never only its creation time (which NTFS can give a newcomer under its name).
    #[cfg(windows)]
    if read_dir_ids(s, t, buf) {
        return;
    }
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
    // Room left under the cap: a big folder hands its matches over in chunks as it's read, so
    // they stream in, and the cap stops the read where it's reached rather than after it.
    let mut room = walk.room();
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
                // Unix: one lstat per folder, for its device and its identity.
                let look = cfg!(unix);
                if look && meta.is_none() {
                    meta = de.metadata().ok();
                }
                // macOS marks some folders hidden by a flag (~/Library).
                if !spec.show_hidden && cfg!(target_os = "macos") && fs::is_hidden(&name, meta.as_ref()) {
                    continue;
                }
                let first = || match meta.as_ref().and_then(|m| folder_id(&path, m)) {
                    Some(id) => walk.entered.lock().unwrap_or_else(|e| e.into_inner()).insert(id),
                    None => true,
                };
                if (spec.cross_devices || on_devices(meta.as_ref(), devices)) && first() {
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
            mode: meta.as_ref().map_or(0, fs::mode_of),
            // The item the walk saw: actions check it's still the one at this path.
            id: meta.as_ref().and_then(fs::ident),
            os: t.rel.join(&os).into_os_string(),
        });
        if found.len() >= room.min(CHUNK) {
            walk.deliver(std::mem::take(&mut found), std::mem::take(&mut seen), false);
            if walk.stop.load(Relaxed) {
                break;
            }
            room = walk.room();
        }
    }
    queue(s, walk, subdirs);
    #[cfg(test)]
    hook::left(&t.dir, walk.serial);
    walk.deliver(found, seen, false);
}

/// Queues the subfolders a read found, for the walking threads.
fn queue(s: &Shared, walk: &Arc<Walk>, subdirs: Vec<Task>) {
    if subdirs.is_empty() || walk.stop.load(Relaxed) {
        return;
    }
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

/// [`read_dir`] on Windows, through `fs::win_dir::each`: every entry with what a listing shows
/// and its volume and file id. False when the folder can't be read that way before any entry
/// (`std`'s way then reads it, or says why it can't).
#[cfg(windows)]
fn read_dir_ids(s: &Shared, t: &Task, buf: &mut String) -> bool {
    let walk = &t.walk;
    let spec = &walk.spec;
    let (mut found, mut subdirs, mut seen) = (Vec::new(), Vec::new(), 0u64);
    let mut room = walk.room();
    let read = fs::win_dir::each(&t.dir, |e| {
        #[cfg(test)]
        hook::entry(&t.dir, walk.serial);
        if walk.stop.load(Relaxed) {
            return false;
        }
        seen += 1;
        if e.hidden && !spec.show_hidden {
            return true;
        }
        // Not a link: a link to a folder is never entered (no loops); junctions and mounted
        // volumes are links.
        if e.is_dir && !e.is_link {
            let path = t.dir.join(&e.os);
            if !walk.skip.contains(&path) {
                subdirs.push(Task { walk: walk.clone(), dir: path, rel: t.rel.join(&e.os) });
            }
        }
        let lower = if spec.query.folds { fold(&e.name, buf) } else { "" };
        if !spec.query.matches(&e.name, lower) {
            return true;
        }
        let lower = if spec.query.folds { lower.to_string() } else { e.name.to_lowercase() };
        let os = t.rel.join(&e.os).into_os_string();
        found.push(Entry { lower, key: next_key(), os, ..e });
        if found.len() >= room.min(CHUNK) {
            walk.deliver(std::mem::take(&mut found), std::mem::take(&mut seen), false);
            if walk.stop.load(Relaxed) {
                return false;
            }
            room = walk.room();
        }
        true
    });
    if read == Err(false) {
        return false;
    }
    queue(s, walk, subdirs);
    #[cfg(test)]
    hook::left(&t.dir, walk.serial);
    walk.deliver(found, seen, false);
    true
}

/// A folder's identity for [`Walk::entered`]: device and inode from its metadata (Unix);
/// `None` elsewhere (Windows walks never enter junctions or mounted folders: they're links).
pub(super) fn folder_id(path: &Path, m: &std::fs::Metadata) -> Option<crate::noclobber::FileId> {
    #[cfg(test)]
    if let Some(id) = hook::folder_alias(path) {
        return Some(id);
    }
    #[cfg(unix)]
    {
        let _ = path;
        crate::noclobber::file_id(path, m)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, m);
        None
    }
}

/// Whether a folder with metadata `m` is on one of `devices` (unknown counts as on them).
pub(super) fn on_devices(m: Option<&std::fs::Metadata>, devices: &[u64]) -> bool {
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

/// The most matches one walking thread holds before handing them over (see `read_dir`).
pub(super) const CHUNK: usize = 256;

impl Walk {
    /// How many more results fit under the cap.
    pub(super) fn room(&self) -> usize {
        let out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        if out.d.capped { 0 } else { self.spec.max.saturating_sub(out.d.found) }
    }

    /// Adds one folder's matches and count to the shared batch, and wakes the window if it has
    /// taken the last batch (and there's something to show, or progress is due).
    pub(super) fn deliver(&self, mut found: Vec<Entry>, seen: u64, done: bool) {
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
pub(super) fn next_key() -> String {
    pub(super) static KEY: AtomicU64 = AtomicU64::new(1);
    format!("r{}", KEY.fetch_add(1, Relaxed))
}
