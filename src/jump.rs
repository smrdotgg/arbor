//! Fuzzy jump (NORTHSTAR Core): ⌘J opens a box over the window, typing ranks the folders
//! you've been to by how well they match times how often and how lately you went there
//! (frecency, as zoxide does), and Enter goes to the best one.
//!
//! Frecency is user data, which NORTHSTAR allows to be stored, but it costs nothing on the
//! default path:
//!
//! - **Recording**: each navigation that lands sends its folder to one writer thread (started
//!   by the first one), which appends a line to `history.log` beside the settings file. The UI
//!   thread does no disk I/O, and nothing is read at startup.
//! - **Loading**: only when Jump first opens. The writer reads and aggregates the log; the box
//!   waits at most [`OPEN_WAIT`] for it, as the first window does for its folder, then shows
//!   "Loading…" until it lands. The aggregate then stays in memory for the session (it's the
//!   user's history, not a copy of the file system). Past [`COMPACT_AT`] lines the writer
//!   rewrites the log as one line for each of the top [`KEEP`] folders.
//! - **Folders that are gone** aren't checked: going to one says so and drops it (and the log
//!   records the drop). Nothing stats the whole list.
//! - All history I/O runs on that one thread, in order, so a load sees every visit sent before
//!   it. Every append, load (with its compaction) and clear holds the history's lock file, so
//!   two Arbor processes never interleave them: a compaction can't drop another process's
//!   append, and each writes through a temporary file of its own.
//! - Failures are said: a write that fails shows in the status bar (once, until one works
//!   again), and Clear says "cleared" only when the writer has removed the log (and any
//!   compaction's leftover temporary files). At exit the writer gets up to [`DRAIN_WAIT`] to
//!   finish what's queued.
//!
//! The log: one line per event, `<unix seconds>\t<rank>\t<path>` for a visit (rank 1) or a
//! compacted folder, `<unix seconds>\t-\t<path>` for a drop. A last line without its newline
//! (a write cut short) is ignored, and the next append ends it with a NUL first, so it stays
//! invalid. The log is read a line at a time, however long. Paths that aren't UTF-8 or hold a
//! line break aren't recorded.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use rune::{Key, KeyEvent, TextEdit, Tx};

use crate::app::{Nav, State};
use crate::commands::Cmd;
use crate::fs;
use crate::pattern::{Case, Glob};

/// How long opening Jump waits for the history to load before showing "Loading…": one frame
/// at 120 Hz, as the first window waits for its folder.
pub const OPEN_WAIT: Duration = Duration::from_millis(8);
/// The log is compacted when it has more lines than this…
pub const COMPACT_AT: usize = 10_000;
/// …down to one line for each of this many folders, the highest frecency first.
pub const KEEP: usize = 5_000;
/// When the ranks add up to more than this, they all shrink to 90% of it and folders under
/// rank 1 go (zoxide's aging), so old habits fade.
pub const MAX_AGE: f64 = 10_000.0;
/// Rows the box ranks and shows at most.
pub const LIMIT: usize = 500;
/// Rows ↑↓ skip with Page Up / Page Down.
const PAGE: usize = 8;

const HOUR: u64 = 3600;
const DAY: u64 = 24 * HOUR;
const WEEK: u64 = 7 * DAY;

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

// ---- the aggregate -------------------------------------------------------------------------

/// One folder in the history.
#[derive(Clone, Debug)]
pub struct Dir {
    pub path: PathBuf,
    pub rank: f64,
    /// When it was last visited (Unix seconds).
    pub last: u64,
    /// As shown: the home folder as `~`.
    pub shown: String,
    /// `shown` folded for matching: each character lowercased to one character, and `\` as
    /// `/` on Windows, so an index here is a character index in `shown`.
    low: Vec<char>,
    /// Where the last folder name starts in `low`.
    tail: usize,
}

impl Dir {
    fn new(path: PathBuf, rank: f64, last: u64) -> Dir {
        let shown = HOME.with(|h| pretty_under(h, &path));
        let low = fold(&shown);
        let tail = low.iter().rposition(|&c| c == '/').map_or(0, |i| i + 1).min(low.len());
        // A root ("/", "C:\") is all tail.
        let tail = if tail == low.len() { 0 } else { tail };
        Dir { path, rank, last, shown, low, tail }
    }

    /// Rank weighted by how recently it was visited (zoxide's buckets).
    pub fn frecency(&self, now: u64) -> f64 {
        let age = now.saturating_sub(self.last);
        self.rank
            * if age < HOUR {
                4.0
            } else if age < DAY {
                2.0
            } else if age < WEEK {
                0.5
            } else {
                0.25
            }
    }
}

fn fold(s: &str) -> Vec<char> {
    let sep = |c: char| if cfg!(windows) && c == '\\' { '/' } else { c };
    if s.is_ascii() {
        return s.bytes().map(|b| sep(b.to_ascii_lowercase() as char)).collect();
    }
    s.chars().map(|c| sep(c.to_lowercase().next().unwrap_or(c))).collect()
}

thread_local! {
    /// The home folder, read once per thread rather than once per folder of a 5k history.
    static HOME: PathBuf = fs::home();
}

/// `fs::pretty` with the home folder given.
fn pretty_under(home: &Path, p: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        Err(_) => p.display().to_string(),
    }
}

/// Every folder visited, with its rank and last visit.
#[derive(Default, Debug)]
pub struct Frecency {
    dirs: Vec<Dir>,
    index: HashMap<PathBuf, usize>,
    /// The ranks added up, for aging.
    total: f64,
}

/// One thing the log records.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Visit { path: PathBuf, at: u64, rank: f64 },
    Drop { path: PathBuf },
}

impl Event {
    /// The line the log stores it as; `None` for a path that can't be one line of UTF-8.
    fn line(&self, at: u64) -> Option<String> {
        let (path, mid) = match self {
            Event::Visit { path, rank, .. } => (path, if *rank == 1.0 { "1".to_string() } else { format!("{rank:.3}") }),
            Event::Drop { path } => (path, "-".to_string()),
        };
        let p = path.to_str().filter(|p| !p.is_empty() && !p.contains(['\n', '\r']))?;
        Some(format!("{at}\t{mid}\t{p}\n"))
    }

    fn parse(line: &str) -> Option<Event> {
        // A NUL marks a line cut short by a crash (see `append`): never a record.
        if line.contains('\0') {
            return None;
        }
        let mut it = line.splitn(3, '\t');
        let at: u64 = it.next()?.trim().parse().ok()?;
        let mid = it.next()?;
        let path = it.next().filter(|p| !p.is_empty())?;
        let path = PathBuf::from(path);
        if !path.is_absolute() && !path.has_root() {
            return None;
        }
        if mid == "-" {
            return Some(Event::Drop { path });
        }
        let rank: f64 = mid.trim().parse().ok().filter(|r: &f64| r.is_finite() && *r > 0.0)?;
        Some(Event::Visit { path, at, rank })
    }
}

impl Frecency {
    pub fn len(&self) -> usize {
        self.dirs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dirs.is_empty()
    }

    #[cfg(test)]
    pub fn get(&self, path: &Path) -> Option<&Dir> {
        self.index.get(path).map(|&i| &self.dirs[i])
    }

    pub fn apply(&mut self, e: &Event) {
        match e {
            Event::Visit { path, at, rank } => self.visit(path, *at, *rank),
            Event::Drop { path } => {
                self.remove(path);
            }
        }
    }

    fn visit(&mut self, path: &Path, at: u64, rank: f64) {
        match self.index.get(path) {
            Some(&i) => {
                let d = &mut self.dirs[i];
                d.rank += rank;
                d.last = d.last.max(at);
            }
            None => {
                self.index.insert(path.to_path_buf(), self.dirs.len());
                self.dirs.push(Dir::new(path.to_path_buf(), rank, at));
            }
        }
        self.total += rank;
        if self.total > MAX_AGE {
            let k = 0.9 * MAX_AGE / self.total;
            for d in &mut self.dirs {
                d.rank *= k;
            }
            self.dirs.retain(|d| d.rank >= 1.0);
            self.total = self.dirs.iter().map(|d| d.rank).sum();
            self.reindex();
        }
    }

    /// Takes `path` out; true when it was there.
    pub fn remove(&mut self, path: &Path) -> bool {
        let Some(i) = self.index.remove(path) else { return false };
        self.total -= self.dirs.swap_remove(i).rank;
        if let Some(moved) = self.dirs.get(i) {
            self.index.insert(moved.path.clone(), i);
        }
        true
    }

    fn reindex(&mut self) {
        self.index = self.dirs.iter().enumerate().map(|(i, d)| (d.path.clone(), i)).collect();
    }

    /// The aggregate of a log's text, and how many lines it had. A last line without its
    /// newline was cut short (a crash mid-write), so it's left out; so is any line that
    /// doesn't parse.
    pub fn from_log(text: &str) -> (Frecency, usize) {
        Frecency::from_reader(text.as_bytes()).unwrap_or_default()
    }

    /// [`from_log`](Self::from_log), reading a line at a time, so a log of any size is read
    /// whole without holding it in memory.
    pub fn from_reader(mut r: impl std::io::BufRead) -> std::io::Result<(Frecency, usize)> {
        let mut f = Frecency::default();
        let (mut lines, mut buf) = (0, Vec::new());
        loop {
            buf.clear();
            if r.read_until(b'\n', &mut buf)? == 0 || buf.last() != Some(&b'\n') {
                return Ok((f, lines));
            }
            lines += 1;
            let line = String::from_utf8_lossy(&buf);
            if let Some(e) = Event::parse(line.trim_end_matches(['\n', '\r'])) {
                f.apply(&e);
            }
        }
    }

    /// The log rewritten: one line per folder for the top [`KEEP`] by frecency.
    pub fn compacted(&self, now: u64) -> String {
        let mut v: Vec<&Dir> = self.dirs.iter().collect();
        v.sort_by(|a, b| b.frecency(now).total_cmp(&a.frecency(now)));
        v.truncate(KEEP);
        v.iter().filter_map(|d| Event::Visit { path: d.path.clone(), at: d.last, rank: d.rank }.line(d.last)).collect::<Vec<_>>().concat()
    }

    /// The folders matching `query`, best first, at most `limit`. Every word of the query must
    /// appear in the path in order (case doesn't matter); the last word should be in the last
    /// folder name, as zoxide has it ("proj arb" finds `~/projects/arbor`). A path whose last
    /// word only matches a parent still shows, far down. Score: frecency × how well it matched
    /// (the last word starting or being the folder's name counts most).
    pub fn rank(&self, query: &str, now: u64, limit: usize) -> Vec<Hit> {
        let words: Vec<Vec<char>> = query.split_whitespace().map(fold).collect();
        let mut v: Vec<(f64, usize)> =
            self.dirs.iter().enumerate().filter_map(|(i, d)| Some((d.frecency(now) * quality(&words, &d.low, d.tail, None)?, i))).collect();
        let order = |a: &(f64, usize), b: &(f64, usize)| {
            let (da, db) = (&self.dirs[a.1], &self.dirs[b.1]);
            b.0.total_cmp(&a.0).then(da.low.len().cmp(&db.low.len())).then_with(|| da.shown.cmp(&db.shown))
        };
        if v.len() > limit {
            v.select_nth_unstable_by(limit, order);
            v.truncate(limit);
        }
        v.sort_by(order);
        v.into_iter()
            .map(|(score, i)| {
                let d = &self.dirs[i];
                let mut marks = Vec::new();
                quality(&words, &d.low, d.tail, Some(&mut marks));
                Hit { path: d.path.clone(), shown: d.shown.clone(), marks: byte_ranges(&d.shown, &marks), score }
            })
            .collect()
    }

    /// The folders under (or at) the path being typed, by frecency: what go-to in Jump offers.
    pub fn under(&self, typed: &Path, now: u64, limit: usize) -> Vec<Hit> {
        let want = fold(&typed.to_string_lossy());
        let mut v: Vec<&Dir> = self
            .dirs
            .iter()
            .filter(|d| {
                let have = fold(&d.path.to_string_lossy());
                have.len() >= want.len() && have[..want.len()] == want[..]
            })
            .collect();
        v.sort_by(|a, b| b.frecency(now).total_cmp(&a.frecency(now)));
        v.truncate(limit);
        v.into_iter().map(|d| Hit { path: d.path.clone(), shown: d.shown.clone(), marks: Vec::new(), score: d.frecency(now) }).collect()
    }
}

/// `query`'s first match in `low` from `from` on.
fn find(low: &[char], word: &[char], from: usize) -> Option<usize> {
    if word.is_empty() {
        return Some(from.min(low.len()));
    }
    if word.len() > low.len() {
        return None;
    }
    (from..=low.len() - word.len()).find(|&i| low[i..i + word.len()] == *word)
}

/// How well `words` match a folder's folded path (see [`Frecency::rank`]), `None` when they
/// don't. Writes the matched character ranges to `marks` when given.
fn quality(words: &[Vec<char>], low: &[char], tail: usize, mut marks: Option<&mut Vec<(usize, usize)>>) -> Option<f64> {
    let mut q = 1.0;
    let mut pos = 0;
    for (k, w) in words.iter().enumerate() {
        let last = k + 1 == words.len();
        let (at, in_tail) = if last {
            match find(low, w, pos.max(tail)) {
                Some(a) => (a, true),
                None => (find(low, w, pos)?, false),
            }
        } else {
            (find(low, w, pos)?, false)
        };
        let starts_word = at == 0 || !low[at - 1].is_alphanumeric();
        if !last {
            if starts_word {
                q *= 1.1;
            }
        } else if !in_tail {
            q *= 0.2;
        } else if at == tail && at + w.len() == low.len() {
            q *= 2.0;
        } else if at == tail {
            q *= 1.5;
        } else if starts_word {
            q *= 1.2;
        }
        if let Some(m) = marks.as_deref_mut() {
            m.push((at, at + w.len()));
        }
        pos = at + w.len();
    }
    Some(q)
}

/// Character ranges in `s` as byte ranges.
fn byte_ranges(s: &str, chars: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let offs: Vec<usize> = s.char_indices().map(|(b, _)| b).chain([s.len()]).collect();
    chars.iter().filter(|(a, b)| a < b && *b < offs.len()).map(|&(a, b)| (offs[a], offs[b])).collect()
}

/// A row in the box.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub path: PathBuf,
    pub shown: String,
    /// Byte ranges of `shown` that matched, to emphasise.
    pub marks: Vec<(usize, usize)>,
    pub score: f64,
}

// ---- the writer thread --------------------------------------------------------------------

enum Msg {
    Append(String),
    /// Forget everything; the answer says whether it's gone from the disk.
    Clear(Sender<Result<(), String>>),
    Load(Sender<Loaded>, Option<rune::Waker>),
    /// Answered once everything sent before it is done (at exit).
    Flush(Sender<()>),
}

/// What a load sends back.
pub struct Loaded {
    pub agg: Frecency,
    pub error: Option<String>,
}

/// The one thread that touches `history.log`.
struct Store {
    tx: Sender<Msg>,
    /// What went wrong writing, once per run of failures, for the status bar.
    errors: Receiver<String>,
    /// Disk operations it has done (appends, reads, rewrites, removals), for tests.
    #[cfg_attr(not(test), allow(dead_code))]
    io: Arc<AtomicU64>,
}

impl Store {
    fn spawn(log: Option<PathBuf>, waker: Option<rune::Waker>) -> Store {
        let (tx, rx) = std::sync::mpsc::channel::<Msg>();
        let (err_tx, errors) = std::sync::mpsc::channel::<String>();
        let io = Arc::new(AtomicU64::new(0));
        let count = io.clone();
        #[cfg(not(test))]
        if let Ok(mut at_exit) = AT_EXIT.lock() {
            *at_exit = Some(tx.clone());
        }
        std::thread::Builder::new()
            .name("arbor-history".into())
            .spawn(move || {
                // Without a file (tests), the log is kept here.
                let mut memory = String::new();
                // Writes are failing: said once, until one works again.
                let mut failing = false;
                let report = |e: String| {
                    let _ = err_tx.send(e);
                    if let Some(w) = &waker {
                        w();
                    }
                };
                for msg in rx {
                    match msg {
                        Msg::Append(line) => match &log {
                            Some(p) => {
                                count.fetch_add(1, Relaxed);
                                match append(p, &line) {
                                    Ok(()) => failing = false,
                                    Err(e) if !failing => {
                                        failing = true;
                                        report(format!("Couldn't save the Jump history ({}): {e}", fs::pretty(p)));
                                    }
                                    Err(_) => {}
                                }
                            }
                            None => memory.push_str(&line),
                        },
                        Msg::Clear(reply) => {
                            let done = match &log {
                                Some(p) => {
                                    count.fetch_add(1, Relaxed);
                                    clear(p).map_err(|e| e.to_string())
                                }
                                None => {
                                    memory.clear();
                                    Ok(())
                                }
                            };
                            let _ = reply.send(done);
                            if let Some(w) = &waker {
                                w();
                            }
                        }
                        Msg::Load(reply, waker) => {
                            let loaded = match &log {
                                Some(p) => load(p, &count),
                                None => Loaded { agg: Frecency::from_log(&memory).0, error: None },
                            };
                            if reply.send(loaded).is_ok()
                                && let Some(w) = waker
                            {
                                w();
                            }
                        }
                        Msg::Flush(reply) => {
                            let _ = reply.send(());
                        }
                    }
                }
            })
            .expect("can't start the history thread");
        Store { tx, errors, io }
    }
}

/// The history's writer, for [`drain_at_exit`] (set when it starts; not in tests, which make
/// many).
#[cfg(not(test))]
static AT_EXIT: std::sync::Mutex<Option<Sender<Msg>>> = std::sync::Mutex::new(None);

/// How long quitting waits for history writes still queued.
#[cfg_attr(test, allow(dead_code))]
pub const DRAIN_WAIT: Duration = Duration::from_millis(100);

/// Waits at most [`DRAIN_WAIT`] for the writer to finish what's queued: called as the window
/// closes and after the app's loop ends, so the last visits aren't lost (a slow disk costs the
/// exit that long at most, and what's still queued then is lost).
pub fn drain_at_exit() {
    #[cfg(not(test))]
    if let Some(tx) = AT_EXIT.lock().ok().and_then(|t| t.clone()) {
        flush(&tx, DRAIN_WAIT);
    }
}

/// Waits up to `wait` for everything sent on `tx` before this to be done. True when it was.
fn flush(tx: &Sender<Msg>, wait: Duration) -> bool {
    let (done, rx) = std::sync::mpsc::channel();
    tx.send(Msg::Flush(done)).is_ok() && rx.recv_timeout(wait).is_ok()
}

/// Runs `f` holding the history's lock file (`history.lock` beside it), so two Arbor processes
/// never interleave a compaction (read, then rewrite) with an append or a clear: one waits for
/// the other. The lock is the operating system's (`flock`, `LockFileEx`), released when the
/// file closes, even if the process dies.
fn locked<T>(p: &Path, f: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(p.with_extension("lock"))?;
    lock.lock()?;
    f()
}

/// Appends `line`. A last line cut short (a crash mid-write) is first ended with a NUL, so it
/// stays invalid: no path holds a NUL, and a line with one never parses.
fn append(p: &Path, line: &str) -> std::io::Result<()> {
    locked(p, || {
        let mut f = std::fs::OpenOptions::new().read(true).append(true).create(true).open(p)?;
        let mut out = String::with_capacity(line.len() + 2);
        if f.metadata()?.len() > 0 {
            let mut last = [0u8];
            f.seek(SeekFrom::End(-1))?;
            f.read_exact(&mut last)?;
            if last[0] != b'\n' {
                out.push_str("\0\n");
            }
        }
        out.push_str(line);
        f.write_all(out.as_bytes())
    })
}

/// Removes the log and any compaction's temporary files beside it (they hold the same paths).
fn clear(p: &Path) -> std::io::Result<()> {
    locked(p, || {
        match std::fs::remove_file(p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let (Some(dir), Some(name)) = (p.parent(), p.file_name().map(|n| n.to_string_lossy().into_owned())) else { return Ok(()) };
        for de in std::fs::read_dir(dir)?.flatten() {
            let n = de.file_name().to_string_lossy().into_owned();
            if n.starts_with(&format!("{name}.")) && n.ends_with(".tmp") {
                std::fs::remove_file(de.path())?;
            }
        }
        Ok(())
    })
}

/// Reads the whole log, a line at a time (however big it is: nothing is cut off), and compacts
/// it when it's long, all under the lock so no other process's append falls between the read
/// and the rewrite.
fn load(p: &Path, io: &AtomicU64) -> Loaded {
    io.fetch_add(1, Relaxed);
    let res = locked(p, || {
        let f = match std::fs::File::open(p) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Frecency::default(), None)),
            Err(e) => return Err(e),
        };
        let (agg, lines) = Frecency::from_reader(std::io::BufReader::new(f))?;
        let mut error = None;
        if lines > COMPACT_AT {
            io.fetch_add(1, Relaxed);
            if let Err(e) = rewrite(p, &agg.compacted(now())) {
                error = Some(format!("Couldn't compact the Jump history: {e}"));
            }
        }
        Ok((agg, error))
    });
    match res {
        Ok((agg, error)) => Loaded { agg, error },
        Err(e) => Loaded { agg: Frecency::default(), error: Some(format!("Couldn't read the Jump history: {e}")) },
    }
}

/// Replaces the log with `text`, through a temporary file of its own (unique to this process
/// and call) renamed over it.
fn rewrite(p: &Path, text: &str) -> std::io::Result<()> {
    let tmp = temp_for(p);
    let res = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, p));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// A temporary file name beside `p`, never used before by any process: `history.log.<pid>-<n>.tmp`.
fn temp_for(p: &Path) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    p.with_extension(format!("log.{}-{}.tmp", std::process::id(), N.fetch_add(1, Relaxed)))
}

/// Where the history lives: beside the settings file.
pub fn log_path() -> Option<PathBuf> {
    Some(crate::config::path()?.with_file_name("history.log"))
}

// ---- the UI side ---------------------------------------------------------------------------

/// The history as the UI holds it.
pub struct History {
    /// `jump.enabled`: off, nothing is recorded or read.
    pub enabled: bool,
    /// `jump.exclude`: folders that aren't recorded.
    pub exclude: Vec<Glob>,
    /// The log's path (`None`: kept in memory only, as in tests).
    pub(crate) log: Option<PathBuf>,
    store: Option<Store>,
    /// The aggregate, from the first time Jump opened.
    pub agg: Option<Frecency>,
    /// A load in flight.
    loading: Option<Receiver<Loaded>>,
    /// Events sent after the load in flight was asked for (it won't have them).
    since: Vec<Event>,
    /// Aggregates landed, for tests and timing.
    pub loads: u64,
    /// What went wrong with the last load (shown once in the status bar).
    pub error: Option<String>,
    /// A clear the writer hasn't answered yet.
    clearing: Option<Receiver<Result<(), String>>>,
    /// Wakes the window when the writer has something to say (a failed write, a clear done).
    pub(crate) waker: Option<rune::Waker>,
}

impl History {
    pub fn new(log: Option<PathBuf>) -> History {
        History {
            enabled: true,
            exclude: Vec::new(),
            log,
            store: None,
            agg: None,
            loading: None,
            since: Vec::new(),
            loads: 0,
            error: None,
            clearing: None,
            waker: None,
        }
    }

    /// Disk operations so far (none until the writer starts).
    #[cfg(test)]
    pub fn io(&self) -> u64 {
        self.store.as_ref().map_or(0, |s| s.io.load(Relaxed))
    }

    #[cfg(test)]
    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
    }

    fn store(&mut self) -> &Store {
        let log = self.log.clone();
        let waker = self.waker.clone();
        self.store.get_or_insert_with(|| Store::spawn(log, waker))
    }

    /// Sends `e` to the log, and applies it to the aggregate if one is loaded.
    fn send(&mut self, e: Event) {
        if let Some(line) = e.line(now()) {
            let _ = self.store().tx.send(Msg::Append(line));
        }
        if let Some(a) = &mut self.agg {
            a.apply(&e);
        } else if self.loading.is_some() {
            self.since.push(e);
        }
    }

    /// A navigation landed in `path`.
    pub fn record(&mut self, path: &Path) {
        if !self.enabled || self.excluded(path) {
            return;
        }
        self.send(Event::Visit { path: path.to_path_buf(), at: now(), rank: 1.0 });
    }

    pub fn excluded(&self, path: &Path) -> bool {
        if self.exclude.is_empty() {
            return false;
        }
        let s = path.to_string_lossy();
        self.exclude.iter().any(|g| g.matches(&s))
    }

    /// `path` is gone: out of the aggregate and the log.
    pub fn forget(&mut self, path: &Path) {
        self.send(Event::Drop { path: path.to_path_buf() });
    }

    /// Forgets everything: at once in this session, and on the disk when the writer gets to it
    /// ([`History::cleared`] says how that went).
    pub fn clear(&mut self) {
        let (reply, rx) = std::sync::mpsc::channel();
        let _ = self.store().tx.send(Msg::Clear(reply));
        self.clearing = Some(rx);
        self.agg = Some(Frecency::default());
        self.loading = None;
        self.since.clear();
    }

    /// The writer's answer to a clear, once it has one: `Err` when the log is still on the
    /// disk (then the session reads it again next time, rather than pretend it's gone).
    pub fn cleared(&mut self) -> Option<Result<(), String>> {
        let rx = self.clearing.as_ref()?;
        let r = match rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err("the history's writer stopped".into()),
        };
        self.clearing = None;
        if r.is_err() {
            self.agg = None;
        }
        Some(r)
    }

    /// A write that failed since the last look (said once per run of failures).
    pub fn write_error(&mut self) -> Option<String> {
        self.store.as_ref()?.errors.try_recv().ok()
    }

    /// Waits up to `wait` for the writer to finish what's queued. True when it did.
    #[cfg(test)]
    pub fn drain(&self, wait: Duration) -> bool {
        self.store.as_ref().is_none_or(|s| flush(&s.tx, wait))
    }

    /// Asks for the aggregate if it isn't here, waiting up to `wait` for it. True once it's in.
    pub fn load(&mut self, wait: Duration, waker: Option<rune::Waker>) -> bool {
        if self.agg.is_some() {
            return true;
        }
        if self.loading.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = self.store().tx.send(Msg::Load(tx, waker));
            self.loading = Some(rx);
        }
        if let Some(rx) = &self.loading
            && let Ok(l) = rx.recv_timeout(wait)
        {
            self.landed(l);
        }
        self.agg.is_some()
    }

    /// Lands a load that finished since. True when one did.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.loading else { return false };
        match rx.try_recv() {
            Ok(l) => {
                self.landed(l);
                true
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.loading = None;
                false
            }
        }
    }

    fn landed(&mut self, l: Loaded) {
        let mut agg = l.agg;
        for e in self.since.drain(..) {
            agg.apply(&e);
        }
        self.agg = Some(agg);
        self.loading = None;
        self.loads += 1;
        self.error = l.error;
    }
}

// ---- the box -------------------------------------------------------------------------------

pub struct JumpBox {
    pub edit: TextEdit,
    /// The rows for `hits_for`.
    pub hits: Vec<Hit>,
    hits_for: Option<String>,
    /// The cursor row, for the query `sel_for`; a new query starts it over at the top.
    sel: usize,
    sel_for: String,
    /// Waiting for the history to load.
    pub loading: bool,
}

impl JumpBox {
    /// Whether what's typed is a path (go-to) rather than words to match.
    pub fn path_mode(&self) -> bool {
        crate::app::is_path_like(self.edit.text.trim_start())
    }

    pub fn sel(&self) -> usize {
        if self.sel_for == self.edit.text { self.sel.min(self.hits.len().saturating_sub(1)) } else { 0 }
    }
}

impl State {
    pub fn open_jump(&mut self, tx: &mut Tx) {
        self.close_overlays(tx);
        let loaded = self.history.load(OPEN_WAIT, self.waker.clone());
        if let Some(e) = self.history.error.take() {
            self.note = Some(e);
        }
        let mut edit = TextEdit::new("");
        edit.focused = true;
        self.jump = Some(JumpBox { edit, hits: Vec::new(), hits_for: None, sel: 0, sel_for: String::new(), loading: !loaded });
        self.jump_refresh();
    }

    pub fn close_jump(&mut self, tx: &mut Tx) {
        if self.jump.take().is_some() {
            tx.blur();
        }
    }

    /// Ranks again if the query changed or the history landed since.
    pub fn jump_refresh(&mut self) {
        let Some(j) = &mut self.jump else { return };
        j.loading = self.history.agg.is_none();
        if j.hits_for.as_deref() == Some(j.edit.text.as_str()) {
            return;
        }
        let q = j.edit.text.trim();
        let now = now();
        j.hits = if crate::app::is_path_like(q) {
            let typed = fs::expand(q);
            let mut v = vec![Hit { path: typed.clone(), shown: q.to_string(), marks: Vec::new(), score: f64::INFINITY }];
            if let Some(a) = &self.history.agg {
                v.extend(a.under(&typed, now, LIMIT).into_iter().filter(|h| h.path != typed));
            }
            v
        } else {
            self.history.agg.as_ref().map(|a| a.rank(q, now, LIMIT)).unwrap_or_default()
        };
        j.hits_for = Some(j.edit.text.clone());
    }

    /// The field's binding (late writes after it closes go to a scratch field).
    pub fn jump_edit(&mut self) -> &mut TextEdit {
        match &mut self.jump {
            Some(j) => &mut j.edit,
            None => &mut self.palette_sink,
        }
    }

    /// The field's keys, before it edits: ↑↓ and Page Up/Down move, Enter goes, Esc closes.
    pub fn jump_field_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        tx.instant();
        let m = ev.mods;
        if m.cmd || m.ctrl || m.alt {
            return false;
        }
        match ev.key {
            Key::Up => self.jump_move(-1, tx),
            Key::Down => self.jump_move(1, tx),
            Key::PageUp => self.jump_move(-(PAGE as isize), tx),
            Key::PageDown => self.jump_move(PAGE as isize, tx),
            Key::Enter => {
                let sel = self.jump.as_ref().map_or(0, JumpBox::sel);
                self.jump_pick(sel, tx);
            }
            Key::Escape => self.close_jump(tx),
            _ => return false,
        }
        true
    }

    /// The window's keys while the box is open (see `palette_key`): Jump's own chord closes it.
    pub fn jump_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        tx.instant();
        match self.keymap.find(|c| c.matches_tx(ev, tx)) {
            Some(Cmd::Jump) => {
                self.close_jump(tx);
                return true;
            }
            Some(Cmd::Paste) => {
                tx.request_paste();
                return true;
            }
            _ => {}
        }
        if self.jump_field_key(ev, tx) {
            return true;
        }
        if let (Key::Text(t), Some(j)) = (&ev.key, &mut self.jump)
            && !ev.mods.cmd
            && !ev.mods.ctrl
        {
            j.edit.insert(t);
            j.edit.focused = true;
            self.jump_changed(tx);
        }
        true
    }

    /// After the query changed: rank again and show the top row.
    pub fn jump_changed(&mut self, tx: &mut Tx) {
        self.jump_refresh();
        if self.jump.as_ref().is_some_and(|j| !j.hits.is_empty()) {
            tx.scroll_into_view_in("jump-list", row_key(0));
        }
    }

    fn jump_move(&mut self, by: isize, tx: &mut Tx) {
        let Some(j) = &mut self.jump else { return };
        if j.hits.is_empty() {
            return;
        }
        let to = (j.sel() as isize + by).clamp(0, j.hits.len() as isize - 1) as usize;
        j.sel = to;
        j.sel_for = j.edit.text.clone();
        tx.scroll_into_view_in("jump-list", row_key(to));
    }

    /// Enter or a click on row `i`: go there.
    pub fn jump_pick(&mut self, i: usize, tx: &mut Tx) {
        tx.instant();
        let Some(j) = &self.jump else { return };
        let Some(hit) = j.hits.get(i) else { return };
        let (path, typed) = (hit.path.clone(), j.path_mode() && i == 0);
        self.close_jump(tx);
        if typed {
            // Go-to, as typing `/` or `~` in the folder does.
            self.go_path(&path);
            return;
        }
        if path == self.cur().cwd {
            return;
        }
        self.go(path, Nav::New, None);
        self.jump_req = self.cur().pending.as_ref().map(|p| p.req);
    }

    /// A Jump to a folder that isn't there any more: say so and forget it.
    pub(crate) fn jump_missed(&mut self, path: &Path) {
        self.history.forget(path);
        self.note = Some(format!("{} no longer exists, so it was removed from Jump", fs::pretty(path)));
    }

    /// Called from `poll`: the history landed, a clear was answered, or a write failed. True
    /// when the view changes.
    pub(crate) fn poll_history(&mut self) -> bool {
        let mut said = false;
        if let Some(r) = self.history.cleared() {
            self.note = Some(match r {
                Ok(()) => "Jump history cleared".into(),
                Err(e) => format!("Couldn't clear the Jump history, so it's still there: {e}"),
            });
            said = true;
        }
        if let Some(e) = self.history.write_error() {
            self.note = Some(e);
            said = true;
        }
        if !self.history.poll() {
            return said;
        }
        if let Some(e) = self.history.error.take() {
            self.note = Some(e);
        }
        if let Some(j) = &mut self.jump {
            j.hits_for = None;
        }
        self.jump_refresh();
        true
    }

    /// Forgets the whole history: run only from the confirmation dialog (`Cmd::permanent`). It
    /// says "cleared" only once the writer has removed it from the disk (`poll_history`).
    pub(crate) fn clear_jump_history(&mut self) {
        self.history.clear();
        self.note = Some("Clearing Jump history…".into());
    }
}

/// A row's key in the box's list.
pub fn row_key(i: usize) -> String {
    format!("jr-{i}")
}

/// The case `jump.exclude` patterns match paths with: the platform's.
pub fn exclude_globs(patterns: &[String]) -> Vec<Glob> {
    patterns.iter().filter_map(|p| Glob::path(p, Case::platform()).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        if cfg!(windows) { PathBuf::from(format!("C:{}", s.replace('/', "\\"))) } else { PathBuf::from(s) }
    }

    fn visit(f: &mut Frecency, s: &str, at: u64, n: usize) {
        for _ in 0..n {
            f.apply(&Event::Visit { path: p(s), at, rank: 1.0 });
        }
    }

    fn top(f: &Frecency, q: &str, now: u64) -> Vec<PathBuf> {
        f.rank(q, now, 10).into_iter().map(|h| h.path).collect()
    }

    #[test]
    fn frecency_weighs_recent_visits_more() {
        let now = 10 * WEEK;
        let d = |last: u64| Dir::new(p("/x"), 2.0, last).frecency(now);
        assert_eq!(d(now - 10), 8.0);
        assert_eq!(d(now - 2 * HOUR), 4.0);
        assert_eq!(d(now - 2 * DAY), 1.0);
        assert_eq!(d(now - 2 * WEEK), 0.5);
    }

    #[test]
    fn words_match_in_order_and_the_last_one_names_the_folder() {
        let now = 10 * WEEK;
        let mut f = Frecency::default();
        visit(&mut f, "/home/u/projects/arbor", now - 100, 3);
        visit(&mut f, "/home/u/projects/arbor/src", now - 100, 10);
        visit(&mut f, "/home/u/archive/old-projects", now - 100, 5);
        visit(&mut f, "/home/u/projects/rune", now - 100, 1);
        // "proj arb": arbor first, though src is visited more (its name doesn't match "arb").
        assert_eq!(top(&f, "proj arb", now)[0], p("/home/u/projects/arbor"));
        // Order matters: "arb proj" needs arb before proj in the path.
        assert!(top(&f, "arb proj", now).is_empty());
        // A single word: the folder named that, then the deeper one that matches only a parent.
        assert_eq!(top(&f, "arbor", now), [p("/home/u/projects/arbor"), p("/home/u/projects/arbor/src")]);
        // Case doesn't matter.
        assert_eq!(top(&f, "RUNE", now), [p("/home/u/projects/rune")]);
        // Nothing typed: by frecency.
        assert_eq!(top(&f, "", now)[0], p("/home/u/projects/arbor/src"));
        // Frecency still counts among good matches: old-projects (5 visits) ranks above
        // projects/arbor/src for "proj"? No: src's last word doesn't name "proj"; both lose
        // to folders whose own name has it.
        assert_eq!(top(&f, "proj", now)[0], p("/home/u/archive/old-projects"));
    }

    #[test]
    fn matched_characters_are_marked_in_the_shown_path() {
        let now = 1000;
        let mut f = Frecency::default();
        visit(&mut f, "/srv/Projects/Arbor", now, 1);
        let h = &f.rank("proj arb", now, 1)[0];
        let marked: Vec<&str> = h.marks.iter().map(|&(a, b)| &h.shown[a..b]).collect();
        assert_eq!(marked, ["Proj", "Arb"]);
    }

    #[test]
    fn the_log_aggregates_and_skips_a_cut_short_last_line() {
        let a = p("/a").display().to_string();
        let b = p("/b").display().to_string();
        let text = format!("100\t1\t{a}\n200\t1\t{a}\nnot a line\n150\t2.5\t{b}\n300\t1\t{b}");
        let (f, lines) = Frecency::from_log(&text);
        assert_eq!(lines, 4);
        assert_eq!(f.get(&p("/a")).map(|d| (d.rank, d.last)), Some((2.0, 200)));
        assert_eq!(f.get(&p("/b")).map(|d| (d.rank, d.last)), Some((2.5, 150)), "the cut-short visit isn't counted");
        // A drop removes it; a visit after brings it back.
        let (f, _) = Frecency::from_log(&format!("{text}\n400\t-\t{b}\n"));
        assert!(f.get(&p("/b")).is_none());
        let (f, _) = Frecency::from_log(&format!("{text}\n400\t-\t{b}\n500\t1\t{b}\n"));
        assert_eq!(f.get(&p("/b")).map(|d| d.rank), Some(1.0));
        // Garbage, relative paths and bad ranks are skipped.
        let (f, _) = Frecency::from_log("x\ty\tz\n1\t1\trelative\n1\tNaN\t/n\n1\t-3\t/n\n\n");
        assert!(f.is_empty());
    }

    #[test]
    fn ranks_age_so_old_habits_fade() {
        let mut f = Frecency::default();
        visit(&mut f, "/old", 1, 9_000);
        visit(&mut f, "/rare", 1, 1);
        visit(&mut f, "/new", 2, 1_500);
        let total: f64 = f.dirs.iter().map(|d| d.rank).sum();
        assert!(total <= MAX_AGE, "{total}");
        assert!(f.get(&p("/rare")).is_none(), "under rank 1 after aging");
        assert!(f.get(&p("/old")).is_some() && f.get(&p("/new")).is_some());
    }

    #[test]
    fn compaction_keeps_the_top_folders_as_one_line_each() {
        let now = 10 * WEEK;
        let mut f = Frecency::default();
        for i in 0..(KEEP + 50) {
            visit(&mut f, &format!("/d/{i}"), now - i as u64, 1);
        }
        visit(&mut f, "/d/7", now, 1);
        let text = f.compacted(now);
        let (g, lines) = Frecency::from_log(&text);
        assert_eq!((lines, g.len()), (KEEP, KEEP));
        assert_eq!(g.get(&p("/d/7")).map(|d| (d.rank, d.last)), Some((2.0, now)));
        assert!(g.get(&p("/d/0")).is_some() && g.get(&p(&format!("/d/{}", KEEP + 49))).is_none(), "the oldest go");
    }

    #[test]
    fn exclude_globs_keep_folders_out() {
        let mut h = History::new(None);
        h.agg = Some(Frecency::default());
        let tmp = p("/tmp").display().to_string();
        h.exclude = exclude_globs(&[format!("{tmp}/**"), "**/node_modules".into()]);
        h.record(&p("/tmp/x/y"));
        h.record(&p("/tmp"));
        h.record(&p("/w/node_modules"));
        h.record(&p("/w/src"));
        let a = h.agg.as_ref().unwrap();
        assert_eq!(a.len(), 1);
        assert!(a.get(&p("/w/src")).is_some());
        h.enabled = false;
        h.record(&p("/w/other"));
        assert_eq!(h.agg.as_ref().unwrap().len(), 1, "off records nothing");
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-jump-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("history.log")
    }

    /// Waits for the writer to have done `n` disk operations.
    fn settle_io(h: &History, n: u64) {
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while h.io() < n && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(h.io(), n);
    }

    #[test]
    fn visits_append_to_the_log_and_load_back() {
        let log = scratch("append");
        // A cut-short last line from an earlier crash: the next append ends it so it stays
        // invalid, and starts a new line.
        std::fs::write(&log, format!("5\t1\t{}\n6\t1\t{}", p("/a").display(), p("/cut").display())).unwrap();
        let mut h = History::new(Some(log.clone()));
        h.record(&p("/b"));
        h.record(&p("/b"));
        settle_io(&h, 2);
        let text = std::fs::read_to_string(&log).unwrap();
        assert_eq!(text.lines().count(), 4, "{text}");
        assert!(text.ends_with('\n'));
        assert!(h.load(Duration::from_secs(5), None));
        let a = h.agg.as_ref().unwrap();
        assert_eq!(a.get(&p("/b")).map(|d| d.rank), Some(2.0));
        assert!(a.get(&p("/cut")).is_none(), "a line cut short never comes back as a visit");
        assert!(a.get(&p("/a")).is_some());
        // Forgetting records a drop, which the next session's load honours.
        h.forget(&p("/b"));
        settle_io(&h, 4);
        let mut again = History::new(Some(log.clone()));
        assert!(again.load(Duration::from_secs(5), None));
        assert!(again.agg.as_ref().unwrap().get(&p("/b")).is_none());
        // Clear removes the file, and says so once it has.
        again.clear();
        assert!(again.agg.as_ref().unwrap().is_empty());
        assert_eq!(cleared(&mut again), Ok(()));
        assert!(!log.exists());
    }

    /// Waits for the writer's answer to a clear.
    fn cleared(h: &mut History) -> Result<(), String> {
        let until = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(r) = h.cleared() {
                return r;
            }
            assert!(std::time::Instant::now() < until, "the clear was never answered");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// The `.tmp` files beside `log`.
    fn temps(log: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(log.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        v.sort();
        v
    }

    /// Clear takes a compaction's leftover temporary files with it (they hold the same paths).
    #[test]
    fn clear_removes_compaction_leftovers_and_says_cleared_only_when_done() {
        let log = scratch("clear-tmp");
        std::fs::write(&log, format!("5\t1\t{}\n", p("/private").display())).unwrap();
        std::fs::write(log.with_extension("log.tmp"), "an old crash's").unwrap();
        std::fs::write(log.with_extension("log.4242-0.tmp"), "another process's").unwrap();
        let other = log.with_file_name("notes.tmp");
        std::fs::write(&other, "not ours").unwrap();
        let mut h = History::new(Some(log.clone()));
        h.clear();
        assert_eq!(cleared(&mut h), Ok(()));
        assert!(!log.exists());
        assert_eq!(temps(&log), ["notes.tmp"], "only the history's own");
    }

    #[cfg(unix)]
    #[test]
    fn a_clear_that_fails_says_so_and_keeps_the_history_readable() {
        use std::os::unix::fs::PermissionsExt;
        let log = scratch("clear-ro");
        std::fs::write(&log, format!("5\t1\t{}\n", p("/kept").display())).unwrap();
        // The lock file first, so only the removal fails.
        std::fs::write(log.with_extension("lock"), "").unwrap();
        let dir = log.parent().unwrap().to_path_buf();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let root = std::fs::write(dir.join("probe"), "").is_ok();
        let mut s = crate::app::tests::state_in(&dir);
        s.history = History::new(Some(log.clone()));
        let mut tx = Tx::default();
        s.run(Cmd::ClearJump, &mut tx);
        s.answer(true, &mut tx);
        assert_eq!(s.note.as_deref(), Some("Clearing Jump history…"), "not cleared until the writer says so");
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while s.history.clearing.is_some() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
            s.poll(&mut tx);
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        if root {
            return;
        }
        assert!(s.note.as_deref().unwrap().starts_with("Couldn't clear the Jump history"), "{:?}", s.note);
        assert!(log.exists());
        assert!(s.history.agg.is_none(), "read again next time, not shown as empty");
    }

    /// A write that fails (here the log's folder is a file) is said in the status bar, once.
    #[test]
    fn a_failed_write_says_so_once() {
        let log = scratch("write-fails");
        let blocker = log.with_file_name("not-a-folder");
        std::fs::write(&blocker, "").unwrap();
        let mut h = History::new(Some(blocker.join("history.log")));
        h.record(&p("/a"));
        h.record(&p("/b"));
        assert!(h.drain(Duration::from_secs(5)));
        let e = h.write_error().expect("told");
        assert!(e.starts_with("Couldn't save the Jump history"), "{e}");
        assert_eq!(h.write_error(), None, "once, not for every visit");
    }

    /// Visits still queued at exit are written, within the drain's bound.
    #[test]
    fn queued_visits_are_written_before_exit() {
        let log = scratch("drain");
        let mut h = History::new(Some(log.clone()));
        for i in 0..1000 {
            h.record(&p(&format!("/d/{i}")));
        }
        assert!(h.drain(Duration::from_secs(10)));
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1000, "none lost");
    }

    /// Another process holding the history's lock (compacting) holds up an append until it's
    /// done, and each compaction writes through a temporary file of its own.
    #[test]
    fn appends_wait_for_another_process_holding_the_lock() {
        let log = scratch("lock");
        std::fs::write(&log, "").unwrap();
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(log.with_extension("lock")).unwrap();
        lock.lock().unwrap();
        let mut h = History::new(Some(log.clone()));
        h.record(&p("/waits"));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "", "written while another held the lock");
        drop(lock);
        assert!(h.drain(Duration::from_secs(5)));
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
        let names: std::collections::HashSet<PathBuf> = (0..3).map(|_| temp_for(&log)).collect();
        assert_eq!(names.len(), 3, "each rewrite has a name of its own");
        assert!(names.iter().all(|n| n.to_string_lossy().contains(&format!(".{}-", std::process::id()))));
        rewrite(&log, "x\n").unwrap();
        assert!(temps(&log).is_empty(), "no temporary file left behind");
    }

    /// A log bigger than any read limit is read whole, so compacting it keeps its newest visits.
    #[test]
    fn a_huge_log_is_read_to_its_end() {
        /// 70 MB of old visits, then one new one, made as they're read.
        struct Huge {
            left: usize,
            tail: Option<Vec<u8>>,
            line: Vec<u8>,
        }
        impl Read for Huge {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.left == 0 {
                    let Some(t) = self.tail.take() else { return Ok(0) };
                    buf[..t.len()].copy_from_slice(&t);
                    return Ok(t.len());
                }
                let n = (buf.len() / self.line.len()).min(self.left).max(1);
                for k in 0..n {
                    buf[k * self.line.len()..(k + 1) * self.line.len()].copy_from_slice(&self.line);
                }
                self.left -= n;
                Ok(n * self.line.len())
            }
        }
        let line = format!("100\t1\t{}\n", p("/old/folder/with/a/longish/name").display()).into_bytes();
        let left = 70_000_000 / line.len();
        let tail = Some(format!("{}\t1\t{}\n", now(), p("/newest").display()).into_bytes());
        let (agg, lines) = Frecency::from_reader(std::io::BufReader::new(Huge { left, tail, line })).unwrap();
        assert_eq!(lines, left + 1);
        assert!(agg.get(&p("/newest")).is_some(), "the end of the log was read");
    }

    #[test]
    fn a_long_log_is_compacted_when_loaded() {
        let log = scratch("compact");
        let mut text = String::new();
        for i in 0..(COMPACT_AT + 10) {
            text.push_str(&format!("{}\t1\t{}\n", now() - i as u64, p(&format!("/d/{}", i % (KEEP + 100))).display()));
        }
        std::fs::write(&log, &text).unwrap();
        let mut h = History::new(Some(log.clone()));
        assert!(h.load(Duration::from_secs(10), None));
        // Over MAX_AGE in ranks, so aging dropped the folders seen once; the rest stay in the
        // session, and the file keeps the top KEEP of them.
        let n = h.agg.as_ref().unwrap().len();
        assert!(n > 0);
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), n.min(KEEP));
        assert!(temps(&log).is_empty());
    }

    /// Opening waits at most `OPEN_WAIT`; a slow load lands later through `poll`, with the
    /// visits made meanwhile.
    #[test]
    fn a_slow_load_doesnt_hold_up_the_box() {
        let mut h = History::new(None);
        // Occupy the writer so the load waits behind it.
        let (block_tx, block_rx) = std::sync::mpsc::channel::<()>();
        let (tx, rx) = std::sync::mpsc::channel();
        let block_rx = std::sync::Mutex::new(block_rx);
        let _ = h.store().tx.send(Msg::Load(
            tx,
            Some(std::sync::Arc::new(move || {
                let _ = block_rx.lock().unwrap().recv_timeout(Duration::from_secs(5));
            })),
        ));
        let t = std::time::Instant::now();
        assert!(!h.load(OPEN_WAIT, None));
        assert!(t.elapsed() < Duration::from_millis(150), "waited {:?}", t.elapsed());
        assert!(h.is_loading());
        h.record(&p("/during"));
        block_tx.send(()).unwrap();
        drop(rx);
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while !h.poll() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(h.agg.as_ref().unwrap().get(&p("/during")).is_some(), "a visit made while loading isn't lost");
    }

    // ---- through the app -----------------------------------------------------------------

    fn dirs(name: &str, subs: &[&str]) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-jump-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for s in subs {
            std::fs::create_dir_all(d.join(s)).unwrap();
        }
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Startup reads and writes nothing: no writer thread, no aggregate, no load. The first
    /// navigation appends one line, on the writer.
    #[test]
    fn startup_does_no_history_io_and_a_visit_appends() {
        let d = dirs("startup", &["a"]);
        let mut s = crate::app::tests::state_in(&d);
        assert_eq!(s.history.io(), 0);
        assert!(s.history.store.is_none() && s.history.agg.is_none() && !s.history.is_loading());
        let log = d.join("history.log");
        s.history.log = Some(log.clone());
        s.go(d.join("a"), Nav::New, None);
        crate::app::tests::settle_all(&mut s);
        settle_io(&s.history, 1);
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.ends_with(&format!("\t1\t{}\n", d.join("a").display())), "{text}");
        // A reload isn't a visit.
        s.reload();
        crate::app::tests::settle_all(&mut s);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(s.history.io(), 1);
    }

    #[test]
    fn jump_goes_to_the_best_match_and_drops_a_folder_thats_gone() {
        let d = dirs("go", &["projects/arbor", "projects/rune", "gone"]);
        let mut s = crate::app::tests::state_in(&d);
        let mut tx = Tx::default();
        for sub in ["projects/arbor", "projects/rune", "gone", "projects/arbor"] {
            s.go(d.join(sub), Nav::New, None);
            crate::app::tests::settle_all(&mut s);
        }
        s.go(d.clone(), Nav::New, None);
        crate::app::tests::settle_all(&mut s);
        s.run(Cmd::Jump, &mut tx);
        assert!(s.jump.as_ref().is_some_and(|j| !j.loading), "a small history loads within the wait");
        assert_eq!(s.jump.as_ref().unwrap().hits.len(), 4, "everything visited, by frecency");
        s.jump_edit().insert("proj arb");
        s.jump_changed(&mut tx);
        assert_eq!(s.jump.as_ref().unwrap().hits[0].path, d.join("projects/arbor"));
        s.jump_pick(0, &mut tx);
        assert!(s.jump.is_none());
        crate::app::tests::settle_all(&mut s);
        assert_eq!(s.cur().cwd, d.join("projects/arbor"));
        // A folder deleted since: going there says so and forgets it.
        std::fs::remove_dir(d.join("gone")).unwrap();
        s.run(Cmd::Jump, &mut tx);
        s.jump_edit().insert("gone");
        s.jump_changed(&mut tx);
        assert_eq!(s.jump.as_ref().unwrap().hits[0].path, d.join("gone"));
        s.jump_pick(0, &mut tx);
        crate::app::tests::settle_all(&mut s);
        assert_eq!(s.cur().cwd, d.join("projects/arbor"), "stays put");
        assert!(s.note.as_deref().is_some_and(|n| n.contains("no longer exists")), "{:?}", s.note);
        assert!(s.history.agg.as_ref().unwrap().get(&d.join("gone")).is_none());
    }

    /// A path typed in Jump goes there, as go-to does.
    #[test]
    fn a_typed_path_goes_there() {
        let d = dirs("typed", &["x/y"]);
        let mut s = crate::app::tests::state_in(&d);
        let mut tx = Tx::default();
        s.run(Cmd::Jump, &mut tx);
        s.jump_edit().insert(&d.join("x").join("y").display().to_string());
        s.jump_changed(&mut tx);
        assert!(s.jump.as_ref().unwrap().path_mode());
        s.jump_field_key(&KeyEvent { key: Key::Enter, mods: rune::Mods::default() }, &mut tx);
        crate::app::tests::settle_all(&mut s);
        assert_eq!(s.cur().cwd, d.join("x").join("y"));
    }

    #[test]
    fn jump_off_records_nothing_and_cant_open() {
        let d = dirs("off", &["a"]);
        let mut s = crate::app::tests::state_in(&d);
        s.history.enabled = false;
        s.go(d.join("a"), Nav::New, None);
        crate::app::tests::settle_all(&mut s);
        assert!(s.history.store.is_none());
        assert!(s.unavailable(Cmd::Jump).is_some());
    }

    /// Ranking a 5k-folder history for one key: `cargo test --release -- --ignored --nocapture rank_speed`.
    #[test]
    #[ignore = "benchmark"]
    fn rank_speed() {
        let now = now();
        let mut f = Frecency::default();
        let words = ["projects", "arbor", "src", "photos", "2024", "work", "client", "notes", "music", "build"];
        for i in 0..5_000u64 {
            let path =
                format!("/home/user/{}/{}-{i}/{}", words[(i % 10) as usize], words[(i / 10 % 10) as usize], words[(i / 100 % 10) as usize]);
            visit(&mut f, &path, now - i * 600, 1 + (i % 7) as usize);
        }
        for q in ["", "a", "proj arb", "photos 20"] {
            let t = std::time::Instant::now();
            let n = 20;
            for _ in 0..n {
                std::hint::black_box(f.rank(q, now, LIMIT));
            }
            println!("rank {q:?}: {:.2} ms", t.elapsed().as_secs_f64() * 1e3 / n as f64);
        }
    }

    /// Jump's handlers on a 5k history: `cargo test --release -- --ignored --nocapture open_speed`.
    #[test]
    #[ignore = "benchmark"]
    fn open_speed() {
        let d = dirs("open-speed", &[]);
        let mut s = crate::app::tests::state_in(&d);
        let now = now();
        let mut log = String::new();
        let words = ["projects", "arbor", "src", "photos", "2024", "work", "client", "notes", "music", "build"];
        for i in 0..5_000usize {
            let w = |k: usize| words[(i / k) % 10];
            log.push_str(&format!("{}\t1\t{}\n", now - i as u64 * 517, d.join(w(1)).join(format!("{}-{i}", w(10))).join(w(100)).display()));
        }
        let t = std::time::Instant::now();
        let (agg, _) = Frecency::from_log(&log);
        println!("aggregate 5k lines: {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
        s.history.agg = Some(agg);
        let mut tx = Tx::default();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            s.open_jump(&mut tx);
            println!("open (warm): {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
            for c in ["a", "r"] {
                let t = std::time::Instant::now();
                s.jump_key(&KeyEvent { key: Key::Text(c.into()), mods: rune::Mods::default() }, &mut tx);
                println!("type {c}: {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
            }
            s.close_jump(&mut tx);
        }
    }
}
