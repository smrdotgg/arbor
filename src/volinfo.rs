//! The status bar's free space, read off Rune's worker pool: a `statvfs` (Windows:
//! `GetDiskFreeSpaceExW`) can't be interrupted, and on a hung network volume it never returns,
//! so it must never hold a thread that listings need.
//!
//! - **A thread per volume with a query in flight** (`arbor-volume`, below the window's
//!   priority, as the search walkers are), started by a query and ending when that volume has
//!   nothing more waiting: nothing idles. A stuck query holds its own thread only; listings
//!   (Rune's pool) and other volumes' queries go on.
//! - **One query in flight per volume.** A query for a volume whose last one hasn't answered
//!   waits behind it and runs only once it does, so a stuck volume never gets a second thread;
//!   until then the status bar shows the last free space known for the folder, or none.
//! - **Newer supersedes older; interest can go.** Each folder's [`Inbox`] wants one request
//!   (the newest); a waiting query it no longer wants is dropped unread, an answer it no longer
//!   wants is dropped, and a folder that's gone (its tab closed) drops all of them.
//! - **Asked** when a folder's listing lands (its volume is known from the listing's identity)
//!   and after a file operation; never on a timer.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, LazyLock, Mutex, Weak};

/// Which volume a query is for: the listed folder's device (Unix) or volume serial number
/// (Windows), or, before its identity is known, its path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Volume {
    Device(u64),
    Path(PathBuf),
}

impl Volume {
    /// The volume holding `path`, listed with identity `id`.
    pub fn of(path: &std::path::Path, id: Option<crate::noclobber::DirId>) -> Volume {
        match id {
            Some(id) => Volume::Device(id.volume()),
            None => Volume::Path(path.to_path_buf()),
        }
    }
}

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

    fn deliver(&self, (req, path, free): Answer) {
        if !self.wants(req) {
            return;
        }
        *self.got.lock().unwrap_or_else(|e| e.into_inner()) = Some((req, path, free));
        let w = self.waker.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(w) = w {
            w();
        }
    }
}

struct Query {
    volume: Volume,
    path: PathBuf,
    req: u64,
    inbox: Weak<Inbox>,
}

impl Query {
    /// Its folder is still there and still wants it.
    fn wanted(&self) -> bool {
        self.inbox.upgrade().is_some_and(|i| i.wants(self.req))
    }
}

#[derive(Default)]
struct Queue {
    /// Volumes with a query in flight (each on its own thread).
    running: HashSet<Volume>,
    /// Queries waiting for their volume's query in flight to answer.
    waiting: HashMap<Volume, Vec<Query>>,
}

static QUEUE: LazyLock<Mutex<Queue>> = LazyLock::new(Mutex::default);
static REQUEST: AtomicU64 = AtomicU64::new(1);

/// Asks for the free space of `volume`, which holds `path`, for `inbox` (superseding what it
/// asked before). The answer lands in the inbox and wakes it.
pub fn ask(inbox: &Arc<Inbox>, volume: Volume, path: PathBuf) {
    let req = REQUEST.fetch_add(1, Relaxed);
    inbox.want.store(req, Relaxed);
    let query = Query { volume: volume.clone(), path, req, inbox: Arc::downgrade(inbox) };
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    // What's no longer wanted (this inbox's older queries, folders gone) leaves the queue.
    q.waiting.retain(|_, v| {
        v.retain(Query::wanted);
        !v.is_empty()
    });
    if q.running.contains(&volume) {
        q.waiting.entry(volume).or_default().push(query);
        return;
    }
    q.running.insert(volume.clone());
    drop(q);
    let started = std::thread::Builder::new().name("arbor-volume".into()).spawn(move || run(query));
    if started.is_err() {
        QUEUE.lock().unwrap_or_else(|e| e.into_inner()).running.remove(&volume);
    }
}

/// A volume's thread: its query, then whatever waited for it, then it ends.
fn run(mut query: Query) {
    crate::search::background();
    loop {
        if query.wanted() {
            let free = crate::fs::free_space(&query.path);
            if let Some(inbox) = query.inbox.upgrade() {
                inbox.deliver((query.req, query.path.clone(), free));
            }
        }
        let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        let next = q.waiting.get_mut(&query.volume).and_then(|v| {
            v.retain(Query::wanted);
            v.pop()
        });
        match next {
            Some(n) => query = n,
            None => {
                q.waiting.remove(&query.volume);
                q.running.remove(&query.volume);
                return;
            }
        }
    }
}

/// Tests: whether a query for `volume` is in flight.
#[cfg(test)]
pub(crate) fn in_flight(volume: &Volume) -> bool {
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).running.contains(volume)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::free_reads;
    use std::time::{Duration, Instant};

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-volinfo-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn answer(inbox: &Inbox, within: Duration) -> Option<(PathBuf, Option<u64>)> {
        let until = Instant::now() + within;
        loop {
            if let Some(a) = inbox.take() {
                return Some(a);
            }
            if Instant::now() > until {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// A stuck volume holds one thread: a second query for it waits (no second read starts),
    /// another volume's query answers meanwhile, and once the stuck one answers, the newest
    /// query waiting for it runs (an older one it superseded never reads).
    #[test]
    fn a_stuck_volume_never_holds_up_another_or_gets_a_second_thread() {
        let (stuck, other) = (dir("stuck"), dir("other"));
        let go = free_reads::hold(&stuck);
        let (a, b) = (Arc::new(Inbox::default()), Arc::new(Inbox::default()));
        let v = Volume::Path(stuck.clone());
        ask(&a, v.clone(), stuck.clone());
        let until = Instant::now() + Duration::from_secs(10);
        while free_reads::of(&stuck).is_empty() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(free_reads::of(&stuck).len(), 1, "the first read started");
        let sub = stuck.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        ask(&a, v.clone(), sub.clone());
        ask(&b, v.clone(), stuck.clone());
        let c = Arc::new(Inbox::default());
        ask(&c, Volume::Path(other.clone()), other.clone());
        assert!(answer(&c, Duration::from_secs(10)).is_some_and(|(p, n)| p == other && n.is_some()), "another volume answers");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(free_reads::of(&stuck).len(), 1, "no second read for the stuck volume");
        assert!(free_reads::of(&sub).is_empty());
        assert!(in_flight(&v));
        go.send(()).unwrap();
        // `a`'s first answer is no longer wanted; its newer query (`sub`) runs, and `b`'s.
        assert!(answer(&a, Duration::from_secs(10)).is_some_and(|(p, _)| p == sub));
        assert!(answer(&b, Duration::from_secs(10)).is_some_and(|(p, _)| p == stuck));
        let until = Instant::now() + Duration::from_secs(10);
        while in_flight(&v) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!in_flight(&v), "the thread ends when nothing waits");
    }

    /// A folder that forgot its question (navigated away, hidden) gets no answer, and one
    /// that's gone has its waiting queries dropped unread.
    #[test]
    fn interest_can_go() {
        let d = dir("forget");
        let go = free_reads::hold(&d);
        let a = Arc::new(Inbox::default());
        let v = Volume::Path(d.clone());
        ask(&a, v.clone(), d.clone());
        let later = d.join("later");
        std::fs::create_dir_all(&later).unwrap();
        let b = Arc::new(Inbox::default());
        ask(&b, v.clone(), later.clone());
        drop(b);
        a.forget();
        go.send(()).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while in_flight(&v) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(a.take(), None, "forgotten");
        assert!(free_reads::of(&later).is_empty(), "a gone folder's query never reads");
    }
}
