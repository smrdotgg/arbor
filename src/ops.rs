//! File operations: copy, move, trash, and moving exact pairs back (undo). Each runs on its own
//! worker thread and reports progress through atomics; the UI reads them in `poll`.
//!
//! Safety rules: nothing is ever overwritten. A name that's taken gets a free one instead
//! ("name copy" in the same folder, "name 2" elsewhere). Folders can't go into themselves.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    /// Move each `.0` to exactly `.1` (undoing a move, rename or trash).
    MovePairs {
        pairs: Vec<(PathBuf, PathBuf)>,
    },
}

impl Op {
    pub fn verb(&self) -> &'static str {
        match self {
            Op::Copy { .. } => "Copying",
            Op::Move { .. } => "Moving",
            Op::Trash { .. } => "Moving to Trash",
            Op::MovePairs { .. } => "Undoing",
        }
    }
}

/// How to reverse a finished operation.
#[derive(Debug, Clone)]
pub enum Undo {
    /// Move each `.0` back to `.1`.
    Moves(Vec<(PathBuf, PathBuf)>),
    /// Trash these (the copies an operation made).
    Trash(Vec<PathBuf>),
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
    pub undo: Option<Undo>,
    pub errors: Vec<String>,
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

pub fn start(op: Op, waker: Option<rune::Waker>) -> Running {
    let progress = Arc::new(Progress::default());
    let done = Arc::new(Mutex::new(None));
    let verb = op.verb();
    let (p, d) = (progress.clone(), done.clone());
    // Headless runs (scripts, snapshots) run on a virtual clock far faster than real time, so
    // they do the work inline to stay deterministic.
    if rune::tooling::headless_run().is_some() {
        let outcome = Worker { p: &p, waker: None, last_wake: Instant::now() }.run(op);
        *done.lock().expect("fresh mutex") = Some(outcome);
        return Running { verb, progress, done };
    }
    std::thread::spawn(move || {
        let mut w = Worker { p: &p, waker: waker.as_ref(), last_wake: Instant::now() };
        let outcome = w.run(op);
        if let Ok(mut slot) = d.lock() {
            *slot = Some(outcome);
        }
        if let Some(wake) = &waker {
            wake();
        }
    });
    Running { verb, progress, done }
}

/// Runs an operation on this thread (tests).
#[cfg(test)]
#[allow(dead_code)]
pub fn run_now(op: Op) -> Outcome {
    let p = Progress::default();
    Worker { p: &p, waker: None, last_wake: Instant::now() }.run(op)
}

struct Worker<'a> {
    p: &'a Progress,
    waker: Option<&'a rune::Waker>,
    last_wake: Instant,
}

impl Worker<'_> {
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
        let mut out = Outcome { results: Vec::new(), undo: None, errors: Vec::new(), cancelled: false };
        match op {
            Op::Copy { sources, dest } => {
                self.p.items_total.store(sources.len(), Relaxed);
                self.p.bytes_total.store(sources.iter().map(|s| tree_size(s)).sum(), Relaxed);
                for src in &sources {
                    if self.cancelled() {
                        break;
                    }
                    match self.copy_one(src, &dest) {
                        Ok(to) => out.results.push(to),
                        Err(e) => out.errors.push(format!("{}: {e}", name_of(src))),
                    }
                    self.p.items_done.fetch_add(1, Relaxed);
                }
                if !out.results.is_empty() {
                    out.undo = Some(Undo::Trash(out.results.clone()));
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
                                pairs.push((to.clone(), src.clone()));
                            }
                            out.results.push(to);
                        }
                        Err(e) => out.errors.push(format!("{}: {e}", name_of(src))),
                    }
                    self.p.items_done.fetch_add(1, Relaxed);
                }
                if !pairs.is_empty() {
                    out.undo = Some(Undo::Moves(pairs));
                }
            }
            Op::Trash { sources } => {
                self.p.items_total.store(sources.len(), Relaxed);
                let mut pairs = Vec::new();
                let mut restorable = true;
                for src in &sources {
                    if self.cancelled() {
                        break;
                    }
                    match crate::trash::trash(src) {
                        Ok(Some(at)) => pairs.push((at, src.clone())),
                        Ok(None) => restorable = false,
                        Err(e) => out.errors.push(format!("{}: {e}", name_of(src))),
                    }
                    self.p.items_done.fetch_add(1, Relaxed);
                    self.tick(0);
                }
                if restorable && !pairs.is_empty() {
                    out.undo = Some(Undo::Moves(pairs));
                }
            }
            Op::MovePairs { pairs } => {
                self.p.items_total.store(pairs.len(), Relaxed);
                for (from, to) in &pairs {
                    if self.cancelled() {
                        break;
                    }
                    if to.symlink_metadata().is_ok() {
                        out.errors.push(format!("{}: something is already there", to.display()));
                    } else if let Some(parent) = to.parent().filter(|p| !p.exists()) {
                        out.errors.push(format!("{}: folder is gone", parent.display()));
                    } else {
                        match self.rename_or_copy(from, to) {
                            Ok(()) => out.results.push(to.clone()),
                            Err(e) => out.errors.push(format!("{}: {e}", name_of(from))),
                        }
                    }
                    self.p.items_done.fetch_add(1, Relaxed);
                }
            }
        }
        out.cancelled = self.cancelled();
        out
    }

    fn copy_one(&mut self, src: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
        let name = src.file_name().ok_or("no file name")?;
        if src.is_dir() && dest_dir.starts_with(src) {
            return Err("can't copy a folder into itself".into());
        }
        let same_dir = src.parent() == Some(dest_dir);
        let to = free_name(dest_dir, name, same_dir);
        if let Err(e) = self.copy_tree(src, &to) {
            // Don't leave a half-made copy behind.
            remove_tree(&to);
            return Err(e);
        }
        Ok(to)
    }

    fn copy_tree(&mut self, src: &Path, dst: &Path) -> Result<(), String> {
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
                self.copy_tree(&de.path(), &dst.join(de.file_name()))?;
            }
        } else {
            // On macOS this clones on APFS: instant, and no extra space.
            std::fs::copy(src, dst).map_err(|e| e.to_string())?;
            self.tick(meta.len());
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
                if let Err(e) = self.copy_tree(from, to) {
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
    fn copy_tree_and_undo() {
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
    fn move_never_overwrites_and_undoes() {
        let d = tmp("mv");
        fs::create_dir(d.join("into")).unwrap();
        fs::write(d.join("f.txt"), "new").unwrap();
        fs::write(d.join("into/f.txt"), "old").unwrap();
        let o = run_now(Op::Move { sources: vec![d.join("f.txt")], dest: d.join("into") });
        assert_eq!(o.results, vec![d.join("into/f 2.txt")]);
        assert_eq!(fs::read_to_string(d.join("into/f.txt")).unwrap(), "old");
        let Some(Undo::Moves(pairs)) = o.undo else { panic!("no undo") };
        let o = run_now(Op::MovePairs { pairs });
        assert!(o.errors.is_empty());
        assert_eq!(fs::read_to_string(d.join("f.txt")).unwrap(), "new");
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
}
