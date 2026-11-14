//! The status bar's free space, read off Rune's worker pool: a `statvfs` (Windows:
//! `GetDiskFreeSpaceExW`) can't be interrupted, and on a hung network volume it never returns,
//! so it must never hold a thread that listings need.
//!
//! - **At most [`THREADS`] threads** (`satchel-volume`, below the window's priority, as the
//!   search walkers are), started by queries and ending when nothing waits: nothing idles. A
//!   stuck query holds its thread (a blocked call can't be killed, and uses no CPU); listings
//!   (Rune's pool) never wait on them.
//! - **Volumes by device** (Unix `st_dev`, Windows' volume serial number), from the folder's
//!   listing; a folder whose identity isn't known isn't asked about (no free space shown).
//! - **One query in flight per volume**, whose answer goes to every folder that asked about
//!   that volume meanwhile (free space is the volume's). A query for a volume with none in
//!   flight waits for a free thread; while every thread is stuck it doesn't start, and the
//!   folder shows no free space (`Started::Waiting`).
//! - **Newer supersedes older; interest can go.** Each folder's [`Inbox`] wants one request
//!   (the newest): a query nobody wants any more is never started, and an answer nobody wants
//!   (navigated away, the tab hidden or closed) is dropped.
//! - **Asked** when a folder's listing lands and after a file operation; never on a timer.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, LazyLock, Mutex, Weak};

/// The most threads reading free space at once, in the whole app.
pub const THREADS: usize = 4;

/// An answer: the request it answers, the folder asked about, its volume's free bytes.
type Answer = (u64, PathBuf, Option<u64>);

/// Where one folder's answers land, and which request it still wants.
#[derive(Default)]
pub struct Inbox {
    /// The request whose answer is wanted; 0: none.
    want: AtomicU64,
    got: Mutex<Option<Answer>>,
    waker: Mutex<Option<rune::Waker>>,
}

impl Inbox {
    pub fn set_waker(&self, w: rune::Waker) {
        *self.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(w);
    }

    /// No answer is wanted any more (the folder left, its tab was hidden).
    pub fn forget(&self) {
        self.want.store(0, Relaxed);
        self.got.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    /// The wanted answer, once it's in: the folder and its volume's free bytes.
    pub fn take(&self) -> Option<(PathBuf, Option<u64>)> {
        let got = self.got.lock().unwrap_or_else(|e| e.into_inner()).take()?;
        (got.0 == self.want.load(Relaxed)).then_some((got.1, got.2))
    }

    fn wants(&self, req: u64) -> bool {
        req != 0 && self.want.load(Relaxed) == req
    }

    fn deliver(&self, answer: Answer) {
        if !self.wants(answer.0) {
            return;
        }
        *self.got.lock().unwrap_or_else(|e| e.into_inner()) = Some(answer);
        let w = self.waker.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(w) = w {
            w();
        }
    }
}

/// A folder waiting for its volume's free space.
struct Waiter {
    req: u64,
    path: PathBuf,
    inbox: Weak<Inbox>,
}

impl Waiter {
    fn wanted(&self) -> bool {
        self.inbox.upgrade().is_some_and(|i| i.wants(self.req))
    }
}

#[derive(Default)]
struct Queue {
    /// Threads running (at most [`THREADS`]).
    threads: usize,
    /// Volumes with a query in flight, and who gets its answer.
    running: HashMap<u64, Vec<Waiter>>,
    /// Volumes waiting for a thread, oldest first, and who asked.
    waiting: VecDeque<(u64, Vec<Waiter>)>,
}

static QUEUE: LazyLock<Mutex<Queue>> = LazyLock::new(Mutex::default);
static REQUEST: AtomicU64 = AtomicU64::new(1);

/// What [`ask`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Started {
    /// A thread is reading it (or its volume's query in flight will answer it).
    Yes,
    /// Every thread is busy (stuck, perhaps): it waits for one, and may never get one.
    Waiting,
    /// The folder's volume isn't known: not asked.
    Unknown,
}

/// Asks for the free space of the volume `device` (`None`: not known), which holds `path`,
/// for `inbox`, superseding what it asked before. The answer lands in the inbox and wakes it.
pub fn ask(inbox: &Arc<Inbox>, device: Option<u64>, path: PathBuf) -> Started {
    let req = REQUEST.fetch_add(1, Relaxed);
    inbox.want.store(req, Relaxed);
    let Some(device) = device else { return Started::Unknown };
    let waiter = Waiter { req, path, inbox: Arc::downgrade(inbox) };
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    // What nobody wants any more (this inbox's older queries, folders gone) leaves the queue.
    q.waiting.retain_mut(|(_, w)| {
        w.retain(Waiter::wanted);
        !w.is_empty()
    });
    if let Some(w) = q.running.get_mut(&device) {
        w.push(waiter);
        return Started::Yes;
    }
    if let Some((_, w)) = q.waiting.iter_mut().find(|(d, _)| *d == device) {
        w.push(waiter);
        return Started::Waiting;
    }
    if q.threads >= THREADS {
        q.waiting.push_back((device, vec![waiter]));
        return Started::Waiting;
    }
    let path = waiter.path.clone();
    q.running.insert(device, vec![waiter]);
    q.threads += 1;
    drop(q);
    if std::thread::Builder::new().name("satchel-volume".into()).spawn(move || run(device, path)).is_err() {
        let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        q.threads -= 1;
        q.running.remove(&device);
    }
    Started::Yes
}

/// A thread: its volume's query, then whichever volume waited longest, then it ends.
fn run(mut device: u64, mut path: PathBuf) {
    crate::search::background();
    loop {
        let wanted = QUEUE.lock().unwrap_or_else(|e| e.into_inner()).running.get(&device).is_some_and(|w| w.iter().any(Waiter::wanted));
        let free = if wanted { crate::fs::free_space(&path) } else { None };
        let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        for w in q.running.remove(&device).unwrap_or_default() {
            if wanted && let Some(inbox) = w.inbox.upgrade() {
                inbox.deliver((w.req, w.path, free));
            }
        }
        let next = loop {
            match q.waiting.pop_front() {
                Some((d, mut w)) => {
                    w.retain(Waiter::wanted);
                    if let Some(first) = w.first() {
                        break Some((d, first.path.clone(), w));
                    }
                }
                None => break None,
            }
        };
        match next {
            Some((d, p, w)) => {
                q.running.insert(d, w);
                (device, path) = (d, p);
            }
            None => {
                q.threads -= 1;
                return;
            }
        }
    }
}

/// Tests: threads running, and whether `device` has a query in flight.
#[cfg(test)]
pub(crate) fn threads() -> usize {
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).threads
}

/// Tests that hang reads on purpose take this first, so they don't take each other's threads
/// (there are only four, for the whole process).
#[cfg(test)]
pub(crate) fn hanging() -> std::sync::MutexGuard<'static, ()> {
    static HANGING: Mutex<()> = Mutex::new(());
    HANGING.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
pub(crate) fn in_flight(device: u64) -> bool {
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).running.contains_key(&device)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::free_reads;
    use std::time::{Duration, Instant};

    /// Tests use made-up devices far from any real one, so they never share a volume with
    /// another test's real folders.
    const FAKE: u64 = u64::MAX - 1_000;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("satchel-volinfo-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Whether `done` (which may take an answer) said so within 10 s.
    fn until(done: impl Fn() -> bool) -> bool {
        let t = Instant::now() + Duration::from_secs(10);
        loop {
            if done() {
                return true;
            }
            if Instant::now() > t {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// A stuck volume holds one thread: another query for it starts nothing (its answer comes
    /// with the stuck one's), another volume's query answers meanwhile, and the threads end
    /// when nothing waits.
    #[test]
    fn a_stuck_volume_never_holds_up_another_or_gets_a_second_thread() {
        let _one_at_a_time = hanging();
        let (stuck, other) = (dir("stuck"), dir("other"));
        let go = free_reads::hold(&stuck);
        let (a, b, c) = (Arc::new(Inbox::default()), Arc::new(Inbox::default()), Arc::new(Inbox::default()));
        assert_eq!(ask(&a, Some(FAKE), stuck.clone()), Started::Yes);
        assert!(until(|| free_reads::of(&stuck).len() == 1), "the first read started");
        assert_eq!(ask(&b, Some(FAKE), stuck.join("sub")), Started::Yes, "joins the query in flight");
        assert_eq!(ask(&c, Some(FAKE + 1), other.clone()), Started::Yes);
        assert!(until(|| c.take().is_some_and(|(p, n)| p == other && n.is_some())), "another volume answers");
        assert_eq!(free_reads::of(&stuck).len(), 1, "no second read for the stuck volume");
        go.send(()).unwrap();
        assert!(until(|| b.take().is_some()), "the stuck volume's answer goes to both");
        assert!(until(|| !in_flight(FAKE)));
    }

    /// However many volumes are stuck, at most four threads read: further volumes' queries
    /// wait (the folder shows no free space) and start only when a thread comes free; an
    /// answer nobody wants any more (the folder forgot, or is gone) is dropped, and a waiting
    /// query nobody wants is never started.
    #[test]
    fn at_most_four_threads_whatever_is_stuck() {
        let _one_at_a_time = hanging();
        let dirs: Vec<PathBuf> = (0..7).map(|i| dir(&format!("many-{i}"))).collect();
        let gates: Vec<_> = dirs[..5].iter().map(|d| free_reads::hold(d)).collect();
        let inboxes: Vec<Arc<Inbox>> = dirs.iter().map(|_| Arc::new(Inbox::default())).collect();
        let started = |range: std::ops::Range<usize>| range.map(|i| free_reads::of(&dirs[i]).len()).sum::<usize>();
        let mut asked = Vec::new();
        for i in 0..7 {
            asked.push(ask(&inboxes[i], Some(FAKE + 10 + i as u64), dirs[i].clone()));
        }
        // Other tests' queries may hold a thread for a moment, so the four are waited for.
        assert!(until(|| started(0..7) == THREADS), "four reads: {}", started(0..7));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(started(0..7), THREADS, "no fifth thread while four are stuck");
        assert!(threads() <= THREADS);
        assert!(asked[THREADS..].iter().all(|s| *s == Started::Waiting), "{asked:?}");
        // The sixth folder gives up, the seventh is closed: neither is ever read.
        let mut inboxes = inboxes;
        inboxes[5].forget();
        drop(inboxes.pop());
        // The first stuck one comes free: the fifth (stuck too) takes its thread.
        let mut gates = gates.into_iter();
        drop(gates.next());
        assert!(until(|| free_reads::of(&dirs[4]).len() == 1), "the waiting volume got the thread");
        assert!(inboxes[0].take().is_some());
        std::thread::sleep(Duration::from_millis(50));
        assert!(free_reads::of(&dirs[5]).is_empty() && free_reads::of(&dirs[6]).is_empty());
        assert!(threads() <= THREADS);
        drop(gates);
    }

    /// A folder whose volume isn't known isn't asked about.
    #[test]
    fn an_unknown_volume_is_not_asked_about() {
        let d = dir("unknown");
        let a = Arc::new(Inbox::default());
        assert_eq!(ask(&a, None, d.clone()), Started::Unknown);
        std::thread::sleep(Duration::from_millis(20));
        assert!(free_reads::of(&d).is_empty() && a.take().is_none());
    }
}
