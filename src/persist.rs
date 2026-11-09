//! Writing user data (NORTHSTAR: "User data is not a cache"): the Jump history, the sidebar's
//! favorites and the folder-handler backup. What their writers share:
//!
//! - [`replace`]: a file is rewritten through a temporary file beside it ([`temp_beside`])
//!   renamed over it, so a crash never leaves half a file. `Durable` also flushes it to the
//!   disk first, for a backup that must be there before what it records changes.
//! - [`locked`]: a read-then-write of a file holds its lock file, so two Arbor processes never
//!   interleave theirs.
//! - [`Failures`]: a writer thread says the first failure of a run in the status bar, once,
//!   until a write works again.
//! - [`at_exit`] and [`drain_at_exit`]: each writer thread registers how to flush it, and
//!   quitting gives each up to [`DRAIN_WAIT`] to finish what's queued.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

/// How long quitting waits for each writer's queued writes.
pub const DRAIN_WAIT: Duration = Duration::from_millis(100);

/// A temporary name beside `p`, never used before by any process: `<name>.<pid>-<n>.tmp`.
pub fn temp_beside(p: &Path) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let mut name = p.as_os_str().to_owned();
    name.push(format!(".{}-{}.tmp", std::process::id(), N.fetch_add(1, Relaxed)));
    PathBuf::from(name)
}

/// Whether [`replace`] waits for the disk.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// Renamed into place as soon as it's written (the OS flushes it later).
    Plain,
    /// Flushed to the disk before it's renamed into place, and the folder after (Unix).
    Durable,
}

/// Replaces `p` with `data` through a temporary file beside it renamed over it, so `p` is
/// always either the old file or the new one. Makes the folder if needed. The temporary file
/// is removed if anything fails.
pub fn replace(p: &Path, data: &[u8], how: Durability) -> io::Result<()> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = temp_beside(p);
    let written = std::fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(data)?;
        if how == Durability::Durable { f.sync_all() } else { Ok(()) }
    });
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, p)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    #[cfg(unix)]
    if how == Durability::Durable
        && let Some(dir) = p.parent()
    {
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
    Ok(())
}

/// Runs `f` holding `p`'s lock file (`p` with the extension `.lock`, made if needed, as is its
/// folder), so two Arbor processes never interleave a read-then-write of `p`: one waits for
/// the other. The lock is the operating system's (`flock`, `LockFileEx`), released when the
/// file closes, even if the process dies.
pub fn locked<T>(p: &Path, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(p.with_extension("lock"))?;
    lock.lock()?;
    f()
}

/// A writer thread's failures, said once per run: the first failure after a success (or
/// since it started) is sent and wakes the window; the rest of the run is quiet.
pub struct Failures {
    failing: bool,
    tx: Sender<String>,
    waker: Option<rune::Waker>,
}

impl Failures {
    /// The thread's side, and the window's (`try_recv` it in `poll`).
    pub fn new(waker: Option<rune::Waker>) -> (Failures, Receiver<String>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Failures { failing: false, tx, waker }, rx)
    }

    /// Notes how a write went; `say` words a failure that starts a run.
    pub fn note<E>(&mut self, r: Result<(), E>, say: impl FnOnce(E) -> String) {
        match r {
            Ok(()) => self.failing = false,
            Err(e) if !self.failing => {
                self.failing = true;
                let _ = self.tx.send(say(e));
                if let Some(w) = &self.waker {
                    w();
                }
            }
            Err(_) => {}
        }
    }
}

/// Sends a flush request (made by `flush` from the reply's sender) and waits up to `wait` for
/// the writer to answer it, which it does once everything sent before is done. True when it
/// did.
pub fn flush<M>(tx: &Sender<M>, wait: Duration, flush: fn(Sender<()>) -> M) -> bool {
    let (done, rx) = std::sync::mpsc::channel();
    tx.send(flush(done)).is_ok() && rx.recv_timeout(wait).is_ok()
}

type Drain = Box<dyn Fn(Duration) + Send>;

/// The writers to flush at exit, in the order they started.
static AT_EXIT: Mutex<Vec<Drain>> = Mutex::new(Vec::new());

/// Registers a writer thread's flush for [`drain_at_exit`]. Not in tests, which start many.
pub fn at_exit<M: Send + 'static>(tx: &Sender<M>, flush_msg: fn(Sender<()>) -> M) {
    if cfg!(test) {
        return;
    }
    let tx = tx.clone();
    if let Ok(mut all) = AT_EXIT.lock() {
        all.push(Box::new(move |wait| {
            flush(&tx, wait, flush_msg);
        }));
    }
}

/// Gives each writer up to [`DRAIN_WAIT`] to finish what's queued: called as the window closes
/// and after the app's loop ends, so the last changes aren't lost (a slow disk costs the exit
/// that long at most, and what's still queued then is lost).
pub fn drain_at_exit() {
    if let Ok(all) = AT_EXIT.lock() {
        for drain in all.iter() {
            drain(DRAIN_WAIT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-persist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn replace_makes_the_folder_and_leaves_no_temporary_file() {
        let d = scratch("replace");
        let p = d.join("sub").join("file.txt");
        for how in [Durability::Plain, Durability::Durable] {
            replace(&p, b"one", how).unwrap();
            replace(&p, b"two", how).unwrap();
            assert_eq!(std::fs::read(&p).unwrap(), b"two");
        }
        assert_eq!(std::fs::read_dir(p.parent().unwrap()).unwrap().count(), 1, "only the file");
        // A failed rename (the target is a non-empty folder) removes the temporary file.
        let blocked = d.join("blocked");
        std::fs::create_dir_all(blocked.join("x")).unwrap();
        assert!(replace(&blocked, b"x", Durability::Plain).is_err());
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 2, "sub and blocked, nothing else");
    }

    #[test]
    fn temporary_names_are_never_reused() {
        let p = Path::new("/x/history.log");
        let a = temp_beside(p);
        let b = temp_beside(p);
        assert_ne!(a, b);
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("history.log.") && name.ends_with(".tmp"), "{name}");
    }

    #[test]
    fn a_run_of_failures_is_said_once() {
        let (mut f, rx) = Failures::new(None);
        f.note(Err("a"), |e: &str| e.to_string());
        f.note(Err("b"), |e: &str| e.to_string());
        f.note(Ok(()), |e: &str| e.to_string());
        f.note(Err("c"), |e: &str| e.to_string());
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), ["a", "c"]);
    }
}
