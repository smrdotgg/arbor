//! What's in the Trash: emptying it (NORTHSTAR Safety #2: it can't be undone, so it asks
//! first, in Arbor's one dialog), and putting items back where they came from.
//!
//! - **Which trashes.** Linux: the home trash (`$XDG_DATA_HOME/Trash`) and, on every local disk
//!   mounted now ([`crate::mounts::local_points`]), the two the freedesktop.org spec names and
//!   Arbor's own trash uses: `$top/.Trash/$uid` (only if `.Trash` is a sticky folder, not a link)
//!   and `$top/.Trash-$uid`, each a real folder of this user's. macOS: `~/.Trash`, and
//!   `.Trashes/$uid` on each local volume under `/Volumes`. Windows: the shell's Recycle Bin on
//!   every drive (`SHQueryRecycleBinW`, `SHEmptyRecycleBinW`): Windows keeps no folder of it
//!   Arbor could list.
//! - **Counting** ([`Counting`]): when the dialog opens, a thread of its own lists the items
//!   (each with its identity), then adds up their sizes; the dialog shows at once and says the
//!   count when it lands. Closing the dialog stops it. Nothing is kept.
//! - **Safe folders** ([`open`], Unix): each trash folder is opened from the one above it
//!   without following a link, and must be this user's (a disk's shared `.Trash` must be
//!   sticky); so must its `files/` and `info/`. A trash that isn't is never read or emptied.
//!   The count pins each folder's identity ([`Pin`]).
//! - **Emptying** runs on the operation worker (`Op::EmptyTrash`): each trash's folders are
//!   opened again and must be the ones counted; then each item is deleted only while it's
//!   still the item that was counted (its [`Ident`], and on Linux its file handle), relative to
//!   the open `files/` with the same no-follow, never-cross-a-mount delete as Delete
//!   Permanently (`delete.rs`), then its `.trashinfo` relative to the open `info/`. An item
//!   with no identity, or put in the Trash after the count, stays. Esc stops it between entries.
//! - **macOS privacy.** `~/.Trash` can't be read without Full Disk Access on recent macOS
//!   (`EPERM`); the dialog and the result say how to grant it ([`NEEDS_ACCESS`]).
//! - **Put Back** (Linux): an item's `.trashinfo` says where it came from ([`parse_info`]); it
//!   goes back there, or beside it under a free name when that's taken (`ops`). Only from a
//!   trash that passes the same checks ([`original`]); from a disk's trash only to a place on
//!   that disk, reached through real folders on it, never a link out of it. macOS has no
//!   public API for Finder's Put Back (Finder keeps the original location to itself), and
//!   Windows doesn't let Arbor list the Recycle Bin, so neither offers it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, channel};

use super::BIN;
use crate::fs::Ident;

/// One trash folder: `items` holds what's in it (freedesktop's `files/`, macOS's `.Trash`), and
/// `info` the freedesktop.org `.trashinfo` records (none on macOS). Both paths are for showing
/// and for the mount table: on Unix nothing is ever reached through them. Each folder is opened
/// as [`Reach`] says, never following a link, and checked ([`open`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Bin {
    pub items: PathBuf,
    pub info: Option<PathBuf>,
    pub reach: Reach,
}

/// How a trash's folders are reached: `anchor` as the user reaches it (links followed: the
/// home folder, a disk's top folder), then each of `steps` opened relative to the one before
/// without following a link, and checked; the last is the trash itself. A freedesktop.org
/// trash (`split`) keeps its items in `files/` and their records in `info/` inside it, each
/// opened the same way and this user's; a macOS one keeps its items in itself.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(not(unix), allow(dead_code))]
pub struct Reach {
    pub anchor: PathBuf,
    pub steps: Vec<(std::ffi::OsString, Need)>,
    pub split: bool,
    pub uid: u32,
}

/// What a step of a trash's path must be (besides a real folder, not a link to one).
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(not(unix), allow(dead_code))]
pub enum Need {
    /// Any folder (macOS's `.Trashes`, which root owns).
    #[cfg_attr(any(test, not(target_os = "macos")), allow(dead_code))]
    Folder,
    /// This user's.
    Owned,
    /// Sticky (a disk's shared `.Trash`, as the freedesktop.org spec requires).
    Sticky,
}

impl Bin {
    /// A freedesktop.org trash folder `root` of `uid`'s, with `files/` and `info/` inside: the
    /// home trash, or a disk's `.Trash-$uid` (`root`'s folder reached as the user reaches it).
    #[cfg_attr(windows, allow(dead_code))]
    pub fn freedesktop(root: &Path, uid: u32) -> Bin {
        let anchor = root.parent().map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
        let name = root.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        Bin {
            items: root.join("files"),
            info: Some(root.join("info")),
            reach: Reach { anchor, steps: vec![(name, Need::Owned)], split: true, uid },
        }
    }

    /// A disk's `$top/.Trash/$uid`, under a sticky shared `.Trash`.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn shared(top: &Path, uid: u32) -> Bin {
        let root = top.join(".Trash").join(uid.to_string());
        Bin {
            items: root.join("files"),
            info: Some(root.join("info")),
            reach: Reach {
                anchor: top.to_path_buf(),
                steps: vec![(".Trash".into(), Need::Sticky), (uid.to_string().into(), Need::Owned)],
                split: true,
                uid,
            },
        }
    }

    /// A macOS trash: `anchor` then `steps`, its items in the last one itself.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn mac(anchor: &Path, steps: Vec<(std::ffi::OsString, Need)>, uid: u32) -> Bin {
        let items = steps.iter().fold(anchor.to_path_buf(), |p, (n, _)| p.join(n));
        Bin { items, info: None, reach: Reach { anchor: anchor.to_path_buf(), steps, split: false, uid } }
    }
}

/// A trash as it was counted: the identities (device and inode) of each folder on its way, its
/// `files` and its `info`, which must be the same when it's emptied.
#[derive(Clone, Debug, PartialEq)]
pub struct Pin {
    pub bin: Bin,
    folders: Vec<(u64, u64)>,
    info: Option<(u64, u64)>,
}

/// One thing in a Trash, as it was counted: its path, its identity (an item replaced since is
/// never deleted for it), its record when there is one, and its trash.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub path: PathBuf,
    pub id: Option<Ident>,
    pub info: Option<PathBuf>,
    /// Linux: the file system's handle for it (inode and generation), when it gives one: a new
    /// item that got its inode back has another.
    pub handle: Option<Vec<u8>>,
    pub pin: Arc<Pin>,
}

/// Why the Trash couldn't all be read.
#[derive(Clone, Debug, PartialEq)]
pub enum Problem {
    /// macOS refused (`EPERM`) to read this trash folder: Arbor needs Full Disk Access.
    NeedsAccess(PathBuf),
    /// Anything else, in words.
    Unreadable(String),
}

impl Problem {
    pub fn say(&self) -> String {
        match self {
            Problem::NeedsAccess(_) => NEEDS_ACCESS.into(),
            Problem::Unreadable(why) => why.clone(),
        }
    }
}

/// What macOS needs before Arbor can read `~/.Trash`.
pub const NEEDS_ACCESS: &str = "Arbor needs Full Disk Access to see what's in the Trash: open System Settings → Privacy & Security → Full Disk Access, turn on Arbor (or the app you started it from), then quit and reopen Arbor";

/// The trash folder the sidebar's "Trash" opens: Linux's home trash `files/`, macOS's
/// `~/.Trash`; Windows has none (the Recycle Bin is a shell folder). A path only, no disk I/O.
pub fn place() -> Option<PathBuf> {
    #[cfg(all(unix, not(target_os = "macos")))]
    return Some(super::home_trash().join("files"));
    #[cfg(target_os = "macos")]
    return Some(crate::fs::home().join(".Trash"));
    #[cfg(windows)]
    None
}

/// Emptying uses the shell's Recycle Bin (Windows, outside tests) rather than listing folders.
pub const SHELL: bool = cfg!(all(windows, not(test)));

/// The trash folders Arbor knows about (on the worker: it looks at each disk's top folder).
/// Tests get [`FIXTURE`] instead, so no test ever touches the user's own Trash.
pub fn bins() -> Vec<Bin> {
    #[cfg(test)]
    return FIXTURE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    #[cfg(not(test))]
    platform_bins()
}

/// This user's id (tests: the owner trash folders must have).
#[cfg(test)]
pub fn me() -> u32 {
    #[cfg(unix)]
    // SAFETY: getuid can't fail.
    return unsafe { libc::getuid() };
    #[cfg(not(unix))]
    0
}

/// Tests' trash folders (empty unless a test sets them).
#[cfg(test)]
pub static FIXTURE: std::sync::Mutex<Vec<Bin>> = std::sync::Mutex::new(Vec::new());

#[cfg(all(unix, not(target_os = "macos"), not(test)))]
fn platform_bins() -> Vec<Bin> {
    // SAFETY: getuid can't fail.
    bins_in(&super::home_trash(), &crate::mounts::local_points(), unsafe { libc::getuid() })
}

#[cfg(all(target_os = "macos", not(test)))]
fn platform_bins() -> Vec<Bin> {
    // SAFETY: getuid can't fail.
    let uid = unsafe { libc::getuid() };
    let mut v = vec![Bin::mac(&crate::fs::home(), vec![(".Trash".into(), Need::Owned)], uid)];
    for top in crate::mounts::local_points().into_iter().filter(|t| t.starts_with("/Volumes")) {
        let b = Bin::mac(&top, vec![(".Trashes".into(), Need::Folder), (uid.to_string().into(), Need::Owned)], uid);
        if real_dir(&b.items, Some(uid)) {
            v.push(b);
        }
    }
    v
}

#[cfg(all(windows, not(test)))]
fn platform_bins() -> Vec<Bin> {
    Vec::new()
}

/// Whether `p` is a folder itself (not a link to one), and this user's when `uid` is given.
#[cfg(unix)]
fn real_dir(p: &Path, uid: Option<u32>) -> bool {
    use std::os::unix::fs::MetadataExt;
    p.symlink_metadata().is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink() && uid.is_none_or(|u| m.uid() == u))
}

/// The freedesktop.org trashes: `home` (the home trash) and, on each disk mounted at one of
/// `tops`, `$top/.Trash/$uid` (only under a sticky `.Trash` that isn't a link) and
/// `$top/.Trash-$uid`, each a real folder owned by `uid` with a real `files` folder. A trash
/// reached twice (a bind mount) is listed once. (Each is checked again, without following
/// links, whenever it's read or emptied: [`open`].)
#[cfg(unix)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn bins_in(home: &Path, tops: &[PathBuf], uid: u32) -> Vec<Bin> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = vec![Bin::freedesktop(home, uid)];
    let mut seen: Vec<Ident> = crate::fs::ident_at(&home.join("files")).into_iter().collect();
    for top in tops {
        let shared = top.join(".Trash");
        let admin = shared
            .symlink_metadata()
            .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink() && m.permissions().mode() & 0o1000 != 0)
            .then(|| Bin::shared(top, uid));
        for bin in admin.into_iter().chain([Bin::freedesktop(&top.join(format!(".Trash-{uid}")), uid)]) {
            let root = bin.items.parent().unwrap_or(&bin.items).to_path_buf();
            if !real_dir(&root, Some(uid)) || !real_dir(&bin.items, None) {
                continue;
            }
            let Some(id) = crate::fs::ident_at(&bin.items) else { continue };
            if seen.contains(&id) {
                continue;
            }
            seen.push(id);
            out.push(bin);
        }
    }
    out
}

/// A trash's folders, opened ([`open`]).
#[cfg(unix)]
pub struct Opened {
    pub files: std::os::fd::OwnedFd,
    pub info: Option<std::os::fd::OwnedFd>,
    pub pin: Pin,
}

/// Opens `bin`'s folders as its [`Reach`] says: each step without following a link and checked
/// (this user's, or sticky), then `files/` and `info/`, each a real folder of this user's.
/// `Ok(None)`: there's no such trash (nothing to empty). A step that's a link, someone else's,
/// or not sticky when it must be is a problem, and nothing in that trash is read or deleted.
#[cfg(unix)]
pub fn open(bin: &Bin) -> Result<Option<Opened>, Problem> {
    use crate::dirfd::{fstat, node, open_dir};
    let r = &bin.reach;
    let mac = cfg!(target_os = "macos");
    let missing = |e: &io::Error| e.kind() == io::ErrorKind::NotFound;
    let not_real = |at: &Path| {
        Problem::Unreadable(format!(
            "“{}” isn't a folder of yours (a link, or someone else's), so Arbor won't empty that Trash",
            crate::fs::pretty(at)
        ))
    };
    let mut at = r.anchor.clone();
    let mut fd = match crate::ffi::cstr(&r.anchor).and_then(|a| open_dir(libc::AT_FDCWD, &a, true)) {
        Ok(fd) => fd,
        Err(e) if missing(&e) => return Ok(None),
        Err(e) => return Err(problem_of(&at, &e, mac)),
    };
    let mut folders = Vec::new();
    #[allow(clippy::unnecessary_cast)] // mode_t is u16 on macOS
    let check = |fd: &std::os::fd::OwnedFd, need: Need, at: &Path| -> Result<(u64, u64), Problem> {
        let st = fstat(fd).map_err(|e| problem_of(at, &e, mac))?;
        let ok = match need {
            Need::Folder => true,
            Need::Owned => st.st_uid == r.uid,
            Need::Sticky => st.st_mode as u32 & libc::S_ISVTX as u32 != 0,
        };
        if ok { Ok(node(&st)) } else { Err(not_real(at)) }
    };
    // Each step: not a link (O_NOFOLLOW, O_DIRECTORY), and what it must be.
    let step =
        |dir: &std::os::fd::OwnedFd, name: &std::ffi::OsStr, need: Need, at: &Path| -> Result<Option<std::os::fd::OwnedFd>, Problem> {
            let c = crate::ffi::cstr(name).map_err(|e| problem_of(at, &e, mac))?;
            match open_dir(std::os::fd::AsRawFd::as_raw_fd(dir), &c, false) {
                Ok(fd) => check(&fd, need, at).map(|_| Some(fd)),
                Err(e) if missing(&e) => Ok(None),
                Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => Err(not_real(at)),
                Err(e) => Err(problem_of(at, &e, mac)),
            }
        };
    for (name, need) in &r.steps {
        at.push(name);
        let Some(next) = step(&fd, name, *need, &at)? else { return Ok(None) };
        folders.push(check(&next, *need, &at)?);
        fd = next;
    }
    if !r.split {
        let files = open_dir(std::os::fd::AsRawFd::as_raw_fd(&fd), c".", false).map_err(|e| problem_of(&at, &e, mac))?;
        return Ok(Some(Opened { files, info: None, pin: Pin { bin: bin.clone(), folders, info: None } }));
    }
    let Some(files) = step(&fd, "files".as_ref(), Need::Owned, &at.join("files"))? else { return Ok(None) };
    folders.push(check(&files, Need::Owned, &at.join("files"))?);
    let info = step(&fd, "info".as_ref(), Need::Owned, &at.join("info"))?;
    let info_id = match &info {
        Some(i) => Some(check(i, Need::Owned, &at.join("info"))?),
        None => None,
    };
    Ok(Some(Opened { files, info, pin: Pin { bin: bin.clone(), folders, info: info_id } }))
}

/// Opens the trash `pin` was counted in again, only if each of its folders is still the one
/// counted (a folder moved away and another, or a link, put in its place is refused).
#[cfg(unix)]
pub fn reopen(pin: &Pin) -> Result<Opened, String> {
    let gone = || "it was moved or replaced since it was counted".to_string();
    match open(&pin.bin) {
        Ok(Some(o)) if o.pin.folders == pin.folders && (pin.info.is_none() || o.pin.info == pin.info) => Ok(o),
        Ok(_) => Err(gone()),
        Err(p) => Err(p.say()),
    }
}

/// Why folder `dir` couldn't be read: on macOS a refusal (`EPERM`, `EACCES`) means Arbor
/// needs Full Disk Access.
pub fn problem_of(dir: &Path, e: &io::Error, mac: bool) -> Problem {
    if mac && e.kind() == io::ErrorKind::PermissionDenied {
        Problem::NeedsAccess(dir.to_path_buf())
    } else {
        Problem::Unreadable(format!("Arbor couldn't read “{}” ({e})", crate::fs::pretty(dir)))
    }
}

/// Test-only: the listing of this folder fails after this many entries, as a dying disk or a
/// network share might partway through.
#[cfg(test)]
pub(crate) static FAIL_AFTER: std::sync::Mutex<Option<(PathBuf, usize)>> = std::sync::Mutex::new(None);

/// Test-only: whether the listing of `dir` should fail now, after `seen` entries.
#[cfg(test)]
fn fail_now(dir: &Path, seen: usize) -> bool {
    FAIL_AFTER.lock().unwrap().as_ref().is_some_and(|(d, n)| d == dir && seen >= *n)
}

/// What's in `bins`: each entry of each one's folder, with its identity and record (macOS's
/// `.DS_Store`, Finder's own, isn't an item). A missing folder is an empty trash; the first
/// folder that can't be read (or isn't safe to: [`open`]) is the problem. A folder whose
/// listing fails partway is left out whole: what it had listed isn't emptied either, so "what's
/// in it stays" is true of all of it. `None` when `stop` says so. On Unix each trash is read
/// through its opened folders, never by path.
#[cfg(unix)]
pub fn list(bins: &[Bin], stop: &dyn Fn() -> bool) -> Option<(Vec<Item>, Option<Problem>)> {
    use std::os::unix::ffi::OsStrExt;
    let (mut items, mut problem) = (Vec::new(), None);
    for b in bins {
        let o = match open(b) {
            Ok(Some(o)) => o,
            Ok(None) => continue,
            Err(p) => {
                problem.get_or_insert(p);
                continue;
            }
        };
        let pin = Arc::new(o.pin.clone());
        let names = match crate::dirfd::Names::open(&o.files) {
            Ok(n) => n,
            Err(e) => {
                problem.get_or_insert(problem_of(&b.items, &e, cfg!(target_os = "macos")));
                continue;
            }
        };
        let mut here = Vec::new();
        let mut whole = true;
        for n in names {
            if stop() {
                return None;
            }
            #[cfg(test)]
            let n = if fail_now(&b.items, here.len()) { Err(io::Error::other("the disk went away")) } else { n };
            let n = match n {
                Ok(n) => n,
                Err(e) => {
                    problem.get_or_insert(problem_of(&b.items, &e, cfg!(target_os = "macos")));
                    whole = false;
                    break;
                }
            };
            let name = std::ffi::OsStr::from_bytes(n.as_bytes());
            if !b.reach.split && name == ".DS_Store" {
                continue;
            }
            let info = b.info.as_ref().map(|i| {
                let mut r = name.to_os_string();
                r.push(".trashinfo");
                i.join(r)
            });
            let id = crate::dirfd::ident_at(&o.files, &n).ok();
            let handle = crate::dirfd::handle_at(&o.files, &n);
            here.push(Item { path: b.items.join(name), id, info, handle, pin: pin.clone() });
        }
        if whole {
            items.extend(here);
        }
    }
    Some((items, problem))
}

/// [`list`] by path where there are no descriptor-relative calls (Windows, in tests only: its
/// Recycle Bin is the shell's).
#[cfg(not(unix))]
pub fn list(bins: &[Bin], stop: &dyn Fn() -> bool) -> Option<(Vec<Item>, Option<Problem>)> {
    let (mut items, mut problem) = (Vec::new(), None);
    for b in bins {
        let pin = Arc::new(Pin { bin: b.clone(), folders: Vec::new(), info: None });
        let rd = match std::fs::read_dir(&b.items) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                problem.get_or_insert(problem_of(&b.items, &e, false));
                continue;
            }
        };
        let mut here = Vec::new();
        let mut whole = true;
        for de in rd {
            if stop() {
                return None;
            }
            #[cfg(test)]
            let de = if fail_now(&b.items, here.len()) { Err(io::Error::other("the disk went away")) } else { de };
            let de = match de {
                Ok(de) => de,
                Err(e) => {
                    problem.get_or_insert(problem_of(&b.items, &e, false));
                    whole = false;
                    break;
                }
            };
            let name = de.file_name();
            if !b.reach.split && name == ".DS_Store" {
                continue;
            }
            let path = de.path();
            let info = b.info.as_ref().map(|i| {
                let mut n = name.clone();
                n.push(".trashinfo");
                i.join(n)
            });
            here.push(Item { id: crate::fs::ident_at(&path), path, info, handle: None, pin: pin.clone() });
        }
        if whole {
            items.extend(here);
        }
    }
    Some((items, problem))
}

/// The bytes in `items`' files, not following links or entering another disk, and whether all
/// of it could be read. `None` when `stop` says so.
pub fn bytes(items: &[Item], stop: &dyn Fn() -> bool) -> Option<(u64, bool)> {
    fn walk(p: &Path, fence: &crate::mounts::Fence, stop: &dyn Fn() -> bool, n: &mut u64, whole: &mut bool) -> Option<()> {
        if stop() {
            return None;
        }
        let Ok(m) = p.symlink_metadata() else {
            *whole = false;
            return Some(());
        };
        if m.is_dir() && !m.file_type().is_symlink() {
            if fence.crosses(p, &m) {
                return Some(());
            }
            let Ok(rd) = std::fs::read_dir(p) else {
                *whole = false;
                return Some(());
            };
            for de in rd {
                match de {
                    Ok(de) => walk(&de.path(), fence, stop, n, whole)?,
                    Err(_) => *whole = false,
                }
            }
        } else if m.is_file() {
            *n += m.len();
        }
        Some(())
    }
    let mounts = crate::mounts::Mounts::read();
    let (mut n, mut whole) = (0, true);
    for it in items {
        walk(&it.path, &crate::mounts::Fence::new(&it.path, &mounts), stop, &mut n, &mut whole)?;
    }
    Some((n, whole))
}

/// What the count found so far.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    /// The items, once listed (not on Windows, where the shell counts).
    pub items: Option<Vec<Item>>,
    /// How many, once known.
    pub count: Option<u64>,
    /// Their size, once added up, and whether all of it could be read.
    pub bytes: Option<(u64, bool)>,
    pub problem: Option<Problem>,
}

enum Counted {
    Listed(Vec<Item>, Option<Problem>),
    Bytes(u64, bool),
    #[cfg_attr(not(windows), allow(dead_code))]
    Totals(u64, u64),
}

/// The count behind Empty Trash's dialog: one thread, started when the dialog opens, that
/// lists the items, then adds up their sizes, waking the window after each, and ends. Dropping
/// it (the dialog closed) stops it at the next entry.
pub struct Counting {
    rx: Receiver<Counted>,
    stop: Arc<AtomicBool>,
    pub got: Summary,
}

impl Counting {
    pub fn start(waker: Option<rune::Waker>) -> Counting {
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let halt = stop.clone();
        let _ = std::thread::Builder::new().name("arbor-trash-count".into()).spawn(move || {
            let wake = || {
                if let Some(w) = &waker {
                    w();
                }
            };
            let stopped = || halt.load(Relaxed);
            if SHELL {
                #[cfg(windows)]
                if let Some((n, b)) = shell_totals() {
                    let _ = tx.send(Counted::Totals(n, b));
                    wake();
                }
                return;
            }
            let Some((items, problem)) = list(&bins(), &stopped) else { return };
            let sized = items.clone();
            if tx.send(Counted::Listed(items, problem)).is_err() {
                return;
            }
            wake();
            if let Some((n, whole)) = bytes(&sized, &stopped)
                && tx.send(Counted::Bytes(n, whole)).is_ok()
            {
                wake();
            }
        });
        Counting { rx, stop, got: Summary::default() }
    }

    /// A count that found `items` and `problem` (tests: a Trash Arbor can't read anywhere).
    #[cfg(test)]
    pub fn found(items: Vec<Item>, problem: Option<Problem>) -> Counting {
        let (tx, rx) = channel();
        let _ = tx.send(Counted::Listed(items, problem));
        Counting { rx, stop: Arc::new(AtomicBool::new(false)), got: Summary::default() }
    }

    /// Takes what the thread found since. True when anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(m) = self.rx.try_recv() {
            changed = true;
            match m {
                Counted::Listed(items, problem) => {
                    self.got.count = Some(items.len() as u64);
                    self.got.items = Some(items);
                    self.got.problem = problem;
                }
                Counted::Bytes(n, whole) => self.got.bytes = Some((n, whole)),
                Counted::Totals(n, b) => {
                    self.got.count = Some(n);
                    self.got.bytes = Some((b, true));
                }
            }
        }
        changed
    }
}

impl Drop for Counting {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

/// What Empty Trash's dialog says and offers, for what the count found.
#[derive(Clone, Debug, PartialEq)]
pub struct Words {
    pub title: String,
    pub body: String,
    /// The destructive button's label; `None` when there's nothing Arbor can empty (the Trash
    /// is empty, or it can't be read), so the dialog only informs and has no such button.
    pub action: Option<&'static str>,
    /// macOS refused to read a trash: the dialog offers "Open Privacy Settings".
    pub settings: bool,
    /// The count hasn't landed: nothing can be emptied yet, but the dialog still asks (its
    /// Cancel says Cancel).
    pub counting: bool,
}

/// The dialog's words for what the count found. Before it lands: counting, and no destructive
/// button (only a count says what the button would delete). Nothing to empty (an empty Trash, or one Arbor can't read): no destructive button. Some
/// trashes unreadable and others not: the count is of what can be emptied, the body names the
/// trash that can't be read, and only the readable ones are emptied.
pub fn ask_words(got: Option<&Summary>) -> Words {
    let action = if cfg!(windows) { "Empty Recycle Bin" } else { "Empty Trash" };
    let size = match got.and_then(|g| g.bytes) {
        Some((b, true)) => format!(" ({})", crate::fs::fmt_size(b)),
        Some((b, false)) => format!(" (at least {})", crate::fs::fmt_size(b)),
        None => String::new(),
    };
    let problem = got.and_then(|g| g.problem.as_ref());
    let settings = matches!(problem, Some(Problem::NeedsAccess(_)));
    let ask = |body: String| Words { title: format!("Empty the {BIN}?"), body, action: Some(action), settings, counting: false };
    let inform = |title: String, body: String| Words { title, body, action: None, settings, counting: false };
    match (got.and_then(|g| g.count), problem) {
        (None, _) => Words {
            title: format!("Empty the {BIN}?"),
            body: format!("Counting what's in the {BIN}… Emptying it deletes everything in it permanently. This can't be undone."),
            action: None,
            settings: false,
            counting: true,
        },
        (Some(0), None) => inform("Nothing to Empty".into(), format!("The {BIN} is already empty.")),
        (Some(0), Some(p)) => inform(format!("Arbor Can't Read the {BIN}"), format!("{}.", p.say())),
        (Some(n), Some(p)) => {
            let unread = match p {
                Problem::NeedsAccess(dir) => format!(
                    "“{}” can't be read, so what's in it stays: Arbor needs Full Disk Access (System Settings → Privacy & Security → Full Disk Access) to empty it.",
                    crate::fs::pretty(dir)
                ),
                Problem::Unreadable(why) => format!("{why}, so what's in it stays."),
            };
            let what = if n == 1 { "The 1 item".to_string() } else { format!("The {}", items_word(n)) };
            ask(format!("{what} Arbor can read in the {BIN}{size} will be deleted permanently. This can't be undone. {unread}"))
        }
        (Some(1), None) => ask(format!("The item in the {BIN}{size} will be deleted permanently. This can't be undone.")),
        (Some(n), None) => ask(format!("The {} in the {BIN}{size} will be deleted permanently. This can't be undone.", items_word(n))),
    }
}

fn items_word(n: u64) -> String {
    if n == 1 { "1 item".into() } else { format!("{} items", crate::search::count(n)) }
}

// ---- Windows: the shell's Recycle Bin -----------------------------------------------------

/// How many items the Recycle Bin holds on every drive, and their size.
#[cfg(windows)]
pub fn shell_totals() -> Option<(u64, u64)> {
    use windows_sys::Win32::UI::Shell::{SHQUERYRBINFO, SHQueryRecycleBinW};
    // SAFETY: a zeroed SHQUERYRBINFO with its size set is what the call fills in.
    let mut info: SHQUERYRBINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHQUERYRBINFO>() as u32;
    // SAFETY: a null root means every drive; `info` is valid.
    let hr = unsafe { SHQueryRecycleBinW(std::ptr::null(), &mut info) };
    (hr >= 0).then(|| (info.i64NumItems.max(0) as u64, info.i64Size.max(0) as u64))
}

/// Empties the Recycle Bin of every drive, after Arbor's own dialog asked:
/// `SHERB_NOCONFIRMATION` (Arbor asked already), `SHERB_NOPROGRESSUI` (Arbor's status bar shows
/// it, and the shell's window would be a second dialog), `SHERB_NOSOUND` (Arbor makes no sounds).
/// What's left is counted again afterwards, so the result says what really happened.
#[cfg(windows)]
pub fn shell_empty() -> Result<String, String> {
    use windows_sys::Win32::UI::Shell::{SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI, SHERB_NOSOUND, SHEmptyRecycleBinW};
    let before = shell_totals();
    if before.is_some_and(|(n, _)| n == 0) {
        return Ok(format!("The {BIN} is already empty"));
    }
    // SAFETY: no window, a null root (every drive), and documented flags.
    let hr =
        unsafe { SHEmptyRecycleBinW(std::ptr::null_mut(), std::ptr::null(), SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND) };
    shell_note(before, hr, shell_totals())
}

/// The result of emptying the Recycle Bin, from its totals `before` and `after` and what the
/// call returned.
#[cfg(any(windows, test))]
pub fn shell_note(before: Option<(u64, u64)>, hr: i32, after: Option<(u64, u64)>) -> Result<String, String> {
    match (before, after) {
        (Some((n, b)), Some((0, _))) => {
            Ok(format!("Emptied the {BIN}: deleted {} ({}) permanently", items_word(n), crate::fs::fmt_size(b)))
        }
        (_, Some((0, _))) => Ok(format!("Emptied the {BIN}")),
        (Some((n, _)), Some((left, _))) if left < n => {
            Err(format!("Emptied part of the {BIN}: deleted {}, {} still in it (code {hr:#010x})", items_word(n - left), items_word(left)))
        }
        (_, Some((left, _))) => Err(format!("Couldn't empty the {BIN}: {} still in it (code {hr:#010x})", items_word(left))),
        (_, None) if hr >= 0 => Ok(format!("Emptied the {BIN}")),
        (_, None) => Err(format!("Couldn't empty the {BIN} (code {hr:#010x})")),
    }
}

// ---- Put Back (freedesktop.org) -------------------------------------------------------------

/// A freedesktop.org trash, from its `files` folder: the trash's own folder (with `info/`) and,
/// for a trash on another disk, that disk's top folder, which relative paths in its records
/// start from.
#[derive(Clone, Debug, PartialEq)]
pub struct TrashDir {
    pub root: PathBuf,
    pub top: Option<PathBuf>,
}

/// Which trash `files` (a folder's path) is the `files/` folder of: the home trash `home`, or a
/// disk's `$top/.Trash-$uid` or `$top/.Trash/$uid`. By its path alone, no disk I/O.
pub fn trash_of(files: &Path, home: Option<&Path>) -> Option<TrashDir> {
    if files.file_name()? != "files" {
        return None;
    }
    let root = files.parent()?;
    if home == Some(root) {
        return Some(TrashDir { root: root.into(), top: None });
    }
    let name = root.file_name()?.to_string_lossy();
    let up = root.parent()?;
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if name.strip_prefix(".Trash-").is_some_and(numeric) {
        return Some(TrashDir { root: root.into(), top: Some(up.into()) });
    }
    if up.file_name()? == ".Trash" && numeric(&name) {
        return Some(TrashDir { root: root.into(), top: Some(up.parent()?.into()) });
    }
    None
}

/// Whether folder `dir` holds trashed items Put Back can restore (Linux only).
pub fn can_put_back_in(dir: &Path) -> bool {
    #[cfg(all(unix, not(target_os = "macos")))]
    return trash_of(dir, Some(&super::home_trash())).is_some();
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        let _ = dir;
        false
    }
}

/// Where an item came from, from its `.trashinfo` text: the `Path=` key of its `[Trash Info]`
/// group, URL-decoded; a relative one is from `top` (a trash on another disk). A path with `..`
/// in it, or none, or two `Path=` lines, is refused rather than guessed.
pub fn parse_info(text: &str, top: Option<&Path>) -> Result<PathBuf, String> {
    let mut in_group = false;
    let mut found = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with('[') {
            in_group = line == "[Trash Info]";
            continue;
        }
        if in_group && let Some(v) = line.strip_prefix("Path=") {
            // Two places: the spec allows one, and other tools may read the other.
            if found.is_some() {
                return Err("its record in the Trash names two places".into());
            }
            found = Some(v);
        }
    }
    let raw = found.ok_or("its record in the Trash doesn't say where it came from")?;
    let bytes = url_decode(raw).ok_or("its record in the Trash names a path Arbor won't trust")?;
    let path = PathBuf::from(os_from_bytes(bytes));
    if path.as_os_str().is_empty() || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err("its record in the Trash names a path Arbor won't trust".into());
    }
    if path.is_absolute() {
        return Ok(path);
    }
    match top {
        Some(top) => Ok(top.join(path)),
        None => Err("its record in the Trash names a relative path".into()),
    }
}

/// Where a trashed item came from, checked, and its record (removed once it's back).
pub struct Origin {
    pub wanted: PathBuf,
    /// The record: its trash's `info/`, opened and checked, and its name there (Unix).
    #[cfg(unix)]
    pub record: (std::os::fd::OwnedFd, std::ffi::CString),
    #[cfg(not(unix))]
    pub record: PathBuf,
}

impl Origin {
    /// Removes the record, now that the item is out of the Trash.
    pub fn forget(&self) -> io::Result<()> {
        #[cfg(unix)]
        let r = crate::dirfd::unlink_at(&self.record.0, &self.record.1);
        #[cfg(not(unix))]
        let r = std::fs::remove_file(&self.record);
        r.or_else(|e| if e.kind() == io::ErrorKind::NotFound { Ok(()) } else { Err(e) })
    }
}

/// Where the trashed item at `trashed` (in a trash's `files/`) came from. Only a trash that
/// passes the freedesktop.org checks counts ([`open`]: this user's `.Trash-$uid`, or `$uid`
/// under a sticky `.Trash`, no links), and its record is read through its opened `info/`. For
/// a disk's trash the place must be on that disk: a relative record starts from its top
/// folder, an absolute one must be inside it, and the folders on the way must be real folders
/// on the same disk, not links out of it (each opened without following one).
pub fn original(trashed: &Path, home: Option<&Path>) -> Result<Origin, String> {
    let t = trashed.parent().and_then(|f| trash_of(f, home)).ok_or("it isn't in a Trash Arbor knows")?;
    let name = trashed.file_name().ok_or("it has no name")?;
    let mut rec = name.to_os_string();
    rec.push(".trashinfo");
    #[cfg(unix)]
    {
        use crate::dirfd::{fstat, open_dir};
        // SAFETY: getuid can't fail.
        let uid = unsafe { libc::getuid() };
        let uid_name = uid.to_string();
        let bin = match &t.top {
            None => Bin::freedesktop(&t.root, uid),
            Some(top) if t.root == top.join(".Trash").join(&uid_name) => Bin::shared(top, uid),
            Some(top) if t.root == top.join(format!(".Trash-{uid}")) => Bin::freedesktop(&t.root, uid),
            Some(_) => return Err("it's in another user's Trash".into()),
        };
        let o = match open(&bin) {
            Ok(Some(o)) => o,
            Ok(None) => return Err("it isn't in a Trash Arbor knows".into()),
            Err(p) => return Err(p.say()),
        };
        // The item really is in the folder that was checked.
        let here = crate::ffi::cstr(name).map_err(|e| e.to_string())?;
        let item = crate::dirfd::ident_at(&o.files, &here).map_err(|e| format!("it couldn't be read ({e})"))?;
        if crate::fs::ident_at(trashed) != Some(item) {
            return Err("it isn't in the Trash folder Arbor checked".into());
        }
        let info = o.info.ok_or("it has no record in the Trash saying where it came from")?;
        let rec = crate::ffi::cstr(&rec).map_err(|e| e.to_string())?;
        let text = read_info_at(&info, &rec)?;
        let wanted = parse_info(&text, t.top.as_deref())?;
        if let Some(top) = &t.top {
            let rel = wanted.strip_prefix(top).map_err(|_| "its record names a place outside the disk its Trash is on".to_string())?;
            let top_fd = crate::ffi::cstr(top).and_then(|c| open_dir(libc::AT_FDCWD, &c, true)).map_err(|e| e.to_string())?;
            let dev = fstat(&top_fd).map_err(|e| e.to_string())?.st_dev;
            let mut fd = top_fd;
            let mut at = top.clone();
            for part in rel.parent().into_iter().flat_map(Path::components) {
                at.push(part);
                let c = crate::ffi::cstr(part.as_os_str()).map_err(|e| e.to_string())?;
                fd = match open_dir(std::os::fd::AsRawFd::as_raw_fd(&fd), &c, false) {
                    Ok(next) => next,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        return Err(format!("the folder it came from, “{}”, isn't there any more", crate::fs::pretty(&at)));
                    }
                    Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
                        return Err(format!("the way back goes through “{}”, which is a link or not a folder", crate::fs::pretty(&at)));
                    }
                    Err(e) => return Err(format!("“{}” couldn't be opened ({e})", crate::fs::pretty(&at))),
                };
                let st = fstat(&fd).map_err(|e| e.to_string())?;
                if st.st_dev != dev {
                    return Err(format!("the way back goes into “{}”, another disk", crate::fs::pretty(&at)));
                }
            }
        }
        Ok(Origin { wanted, record: (info, rec) })
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        let info = t.root.join("info").join(rec);
        let text = read_info(&info)?;
        Ok(Origin { wanted: parse_info(&text, t.top.as_deref())?, record: info })
    }
}

/// A `.trashinfo` file's text, read through its open `info/` folder: a regular file (not a
/// link: `O_NOFOLLOW`; nothing that could block: `O_NONBLOCK`) of at most 64 KB.
#[cfg(unix)]
fn read_info_at(info: &std::os::fd::OwnedFd, rec: &std::ffi::CStr) -> Result<String, String> {
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    // SAFETY: `rec` is NUL-terminated; a non-negative result is a descriptor we own.
    let fd = unsafe { libc::openat(info.as_raw_fd(), rec.as_ptr(), flags) };
    if fd < 0 {
        let e = io::Error::last_os_error();
        return Err(match e.kind() {
            io::ErrorKind::NotFound => "it has no record in the Trash saying where it came from".into(),
            _ if e.raw_os_error() == Some(libc::ELOOP) => "its record in the Trash isn't a file".into(),
            _ => format!("its record in the Trash couldn't be read ({e})"),
        });
    }
    // SAFETY: just opened, owned here.
    let f = unsafe { std::fs::File::from_raw_fd(fd) };
    if !f.metadata().is_ok_and(|m| m.is_file()) {
        return Err("its record in the Trash isn't a file".into());
    }
    let mut s = String::new();
    f.take(64 * 1024).read_to_string(&mut s).map_err(|e| format!("its record in the Trash couldn't be read ({e})"))?;
    Ok(s)
}

/// A `.trashinfo` file's text: a regular file (not a link) of at most 64 KB.
#[cfg(not(unix))]
fn read_info(info: &Path) -> Result<String, String> {
    use std::io::Read;
    match info.symlink_metadata() {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err("it has no record in the Trash saying where it came from".into()),
        Err(e) => return Err(format!("its record in the Trash couldn't be read ({e})")),
        Ok(m) if !m.is_file() => return Err("its record in the Trash isn't a file".into()),
        Ok(_) => {}
    }
    let mut s = String::new();
    std::fs::File::open(info)
        .and_then(|f| f.take(64 * 1024).read_to_string(&mut s))
        .map_err(|e| format!("its record in the Trash couldn't be read ({e})"))?;
    Ok(s)
}

/// `%XX` escapes back to their bytes; `None` for an escaped NUL (no path holds one) or `/` (a
/// separator inside a name, which no name holds either), as GLib refuses them.
fn url_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && let (Some(h), Some(l)) = (b.get(i + 1).and_then(|&c| hex(c)), b.get(i + 2).and_then(|&c| hex(c)))
        {
            let c = (h * 16 + l) as u8;
            if c == 0 || c == b'/' {
                return None;
            }
            out.push(c);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    // A NUL written as it is (not escaped) is no better.
    (!out.contains(&0)).then_some(out)
}

fn os_from_bytes(b: Vec<u8>) -> std::ffi::OsString {
    #[cfg(unix)]
    return <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(b);
    #[cfg(not(unix))]
    std::ffi::OsString::from(String::from_utf8_lossy(&b).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-contents-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Any record text: never a panic, and any place it gives is a full path with no `..` and no
    /// NUL in it, inside the disk's top folder when the record is relative.
    #[test]
    fn any_record_gives_a_safe_place_or_an_error() {
        const BITS: &[&str] = &[
            "[Trash Info]",
            "[Other]",
            "\n",
            "\r\n",
            "Path=",
            "Path =",
            "DeletionDate=x",
            "/",
            "..",
            ".",
            "%2E",
            "%2F",
            "%00",
            "%",
            "%4",
            "%zz",
            "%41",
            "a",
            "é",
            " ",
            "\0",
            "~",
            "\\",
        ];
        let top = Path::new(if cfg!(windows) { r"D:\mnt" } else { "/mnt/usb" });
        crate::prop::check("trashinfo_any", 3000, |r| {
            let text = if r.one_in(4) { r.string(80) } else { r.from(BITS, 30) };
            for t in [None, Some(top)] {
                if let Ok(p) = parse_info(&text, t) {
                    assert!(p.is_absolute(), "{text:?} → {p:?}");
                    assert!(!p.components().any(|c| matches!(c, std::path::Component::ParentDir)), "{text:?} → {p:?}");
                    assert!(!p.as_os_str().as_encoded_bytes().contains(&0), "{text:?} → {p:?}");
                    let relative = text
                        .lines()
                        .find_map(|l| l.trim_end_matches('\r').strip_prefix("Path="))
                        .is_some_and(|v| !v.starts_with(['/', '%']));
                    if relative && let Some(t) = t {
                        assert!(p.starts_with(t), "{text:?} → {p:?}");
                    }
                }
            }
        });
    }

    /// What Arbor's own trash writes reads back as the same place, for any name (Linux).
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn any_path_round_trips_through_a_record() {
        use std::os::unix::ffi::OsStringExt;
        crate::prop::check("trashinfo_round_trip", 2000, |r| {
            let mut b = vec![b'/'];
            for _ in 0..1 + r.below(4) {
                let mut name: Vec<u8> = r.bytes(12).into_iter().filter(|&c| c != 0 && c != b'/').collect();
                if name.is_empty() || name == b".." || name == b"." {
                    name = b"n".to_vec();
                }
                b.extend(name);
                b.push(b'/');
            }
            b.pop();
            let p = PathBuf::from(std::ffi::OsString::from_vec(b));
            let text = format!("[Trash Info]\nPath={}\nDeletionDate=2026-11-09T10:00:00\n", crate::trash::url_escape(&p));
            assert_eq!(parse_info(&text, None).as_ref(), Ok(&p), "{text:?}");
        });
    }

    /// Found by `any_record_gives_a_safe_place_or_an_error`: `%00` (or a raw NUL) put a NUL in
    /// the place to put an item back, and `%2F` hid a separator; both are refused now.
    #[test]
    fn a_record_with_a_nul_or_an_escaped_slash_is_refused() {
        let root = if cfg!(windows) { "C:/" } else { "/" };
        for v in ["a%00b", "a\0b", "a%2F..%2Fb", "a%2fb"] {
            assert!(parse_info(&format!("[Trash Info]\nPath={root}{v}\n"), None).is_err(), "{v:?}");
        }
        assert!(parse_info(&format!("[Trash Info]\nPath={root}a%2Cb\n"), None).is_ok());
    }

    /// Two `Path=` lines in the group: Arbor took the first, where another tool may take the
    /// last and put the item back somewhere else. It's damage, refused.
    #[test]
    fn a_record_naming_two_places_is_refused() {
        let root = if cfg!(windows) { "C:/" } else { "/" };
        assert!(parse_info(&format!("[Trash Info]\nPath={root}a\nPath={root}b\n"), None).is_err());
        assert!(parse_info(&format!("[Trash Info]\nPath={root}a\n[Other]\nPath={root}b\n"), None).is_ok(), "another group's doesn't count");
    }

    /// A giant record is read at most 64 KB (`read_info_at`) and parsed at once. The limit is
    /// generous so a loaded machine can't fail it: anything worse than linear in 3 MB would
    /// take hours, not seconds.
    #[test]
    fn a_giant_record_is_parsed_quickly() {
        let root = if cfg!(windows) { "C:/" } else { "/" };
        let text = format!("[Trash Info]\nPath={root}{}\n", "%41".repeat(1 << 20));
        let p = crate::prop::within(std::time::Duration::from_secs(10), "a 3 MB record", move || parse_info(&text, None));
        assert!(p.is_ok());
    }

    /// The home trash, then each disk's `.Trash/$uid` (only under a sticky `.Trash` that isn't
    /// a link) and `.Trash-$uid` (a real folder, not a link); one reached twice is listed once.
    #[cfg(unix)]
    #[test]
    fn bins_are_the_spec_trashes_and_never_an_unsafe_one() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("bins");
        // SAFETY: getuid can't fail.
        let uid = unsafe { libc::getuid() };
        let home = d.join("home/Trash");
        fs::create_dir_all(home.join("files")).unwrap();
        // A sticky .Trash with this user's folder, and a .Trash-uid beside it.
        let a = d.join("a");
        fs::create_dir_all(a.join(format!(".Trash/{uid}/files"))).unwrap();
        fs::set_permissions(a.join(".Trash"), fs::Permissions::from_mode(0o1777)).unwrap();
        fs::create_dir_all(a.join(format!(".Trash-{uid}/files"))).unwrap();
        // A .Trash that isn't sticky: not safe to share, so not used.
        let b = d.join("b");
        fs::create_dir_all(b.join(format!(".Trash/{uid}/files"))).unwrap();
        fs::set_permissions(b.join(".Trash"), fs::Permissions::from_mode(0o777)).unwrap();
        // A .Trash-uid that's a link to somewhere else: never followed.
        let c = d.join("c");
        fs::create_dir_all(c.join("elsewhere/files")).unwrap();
        std::os::unix::fs::symlink(c.join("elsewhere"), c.join(format!(".Trash-{uid}"))).unwrap();
        let got = bins_in(&home, &[a.clone(), b, c, a.clone()], uid);
        let items: Vec<PathBuf> = got.iter().map(|b| b.items.clone()).collect();
        assert_eq!(items, [home.join("files"), a.join(format!(".Trash/{uid}/files")), a.join(format!(".Trash-{uid}/files"))]);
        assert_eq!(got[1].info, Some(a.join(format!(".Trash/{uid}/info"))));
        let _ = fs::remove_dir_all(&d);
    }

    /// Each entry is an item with its identity and record; a missing trash is an empty one;
    /// macOS's `.DS_Store` (a trash with no records) isn't an item.
    #[test]
    fn list_finds_items_and_their_records() {
        let d = tmp("list");
        fs::create_dir_all(d.join("t/files/folder")).unwrap();
        fs::write(d.join("t/files/a.txt"), "a").unwrap();
        fs::create_dir_all(d.join("mac")).unwrap();
        fs::write(d.join("mac/.DS_Store"), "x").unwrap();
        fs::write(d.join("mac/b"), "b").unwrap();
        let bins = [
            Bin::freedesktop(&d.join("t"), me()),
            Bin::mac(&d, vec![("mac".into(), Need::Owned)], me()),
            Bin::freedesktop(&d.join("missing"), me()),
        ];
        let (mut items, problem) = list(&bins, &|| false).unwrap();
        items.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(problem, None);
        let paths: Vec<&Path> = items.iter().map(|i| i.path.as_path()).collect();
        assert_eq!(paths, [d.join("mac/b"), d.join("t/files/a.txt"), d.join("t/files/folder")]);
        assert_eq!(items[1].info, Some(d.join("t/info/a.txt.trashinfo")));
        assert_eq!(items[0].info, None);
        assert!(items.iter().all(|i| i.id.is_some()));
        assert!(list(&bins, &|| true).is_none(), "a stop stops it");
        let _ = fs::remove_dir_all(&d);
    }

    /// The size counts what's in the items, never what a link inside one points to.
    #[test]
    fn bytes_never_follow_a_link() {
        let d = tmp("bytes");
        fs::create_dir_all(d.join("files/f")).unwrap();
        fs::write(d.join("files/f/x"), vec![0u8; 1000]).unwrap();
        fs::write(d.join("files/y"), vec![0u8; 24]).unwrap();
        fs::write(d.join("big"), vec![0u8; 100_000]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(d.join("big"), d.join("files/f/link")).unwrap();
        let (items, _) = list(&[Bin::mac(&d, vec![("files".into(), Need::Owned)], me())], &|| false).unwrap();
        assert_eq!(bytes(&items, &|| false), Some((1024, true)));
        assert_eq!(bytes(&items, &|| true), None);
        let _ = fs::remove_dir_all(&d);
    }

    /// The dialog says the count and size when known, an empty Trash plainly, and on macOS how
    /// to grant Full Disk Access when the Trash can't be read.
    #[test]
    fn the_dialog_says_what_the_count_found() {
        let w = ask_words(None);
        assert_eq!(w.title, format!("Empty the {BIN}?"));
        assert!(w.body.starts_with("Counting") && w.body.ends_with("This can't be undone."), "{}", w.body);
        assert!(w.action.is_none() && w.counting && !w.settings, "no button before the count");
        let got = |count, bytes, problem| Summary { items: None, count, bytes, problem };
        let say = |g: Summary| ask_words(Some(&g)).body;
        assert_eq!(
            say(got(Some(1234), None, None)),
            format!("The 1,234 items in the {BIN} will be deleted permanently. This can't be undone.")
        );
        assert_eq!(
            say(got(Some(3), Some((2048, true)), None)),
            format!("The 3 items in the {BIN} (2.0 KB) will be deleted permanently. This can't be undone.")
        );
        assert!(say(got(Some(1), Some((10, false)), None)).starts_with(&format!("The item in the {BIN} (at least")));
        let partly = say(got(Some(2), None, Some(Problem::Unreadable("Arbor couldn't read “/x” (denied)".into()))));
        assert!(partly.contains("The 2 items Arbor can read") && partly.contains("This can't be undone."), "{partly}");
        assert!(partly.contains("“/x”"), "names the trash it can't read: {partly}");
    }

    /// Nothing to empty (an empty Trash, or one Arbor can't read): the dialog informs and has no
    /// destructive button. Where macOS refused, it explains Full Disk Access and offers the
    /// settings; where only some trashes refused, it counts what can be emptied, names the
    /// trash it can't read, still says it can't be undone, and keeps the button.
    #[test]
    fn nothing_to_empty_offers_no_destructive_button() {
        let got = |count, problem| Summary { items: None, count, bytes: None, problem };
        let empty = ask_words(Some(&got(Some(0), None)));
        assert_eq!((empty.body.as_str(), empty.action, empty.settings), (&*format!("The {BIN} is already empty."), None, false));
        let home = PathBuf::from("/Users/me/.Trash");
        let refused = ask_words(Some(&got(Some(0), Some(Problem::NeedsAccess(home.clone())))));
        assert_eq!((refused.body, refused.action, refused.settings), (format!("{NEEDS_ACCESS}."), None, true));
        assert!(!refused.title.ends_with('?'), "it asks nothing: {}", refused.title);
        let unreadable = ask_words(Some(&got(Some(0), Some(Problem::Unreadable("Arbor couldn't read “/x” (denied)".into())))));
        assert_eq!((unreadable.action, unreadable.settings), (None, false));
        let partly =
            ask_words(Some(&Summary { items: None, count: Some(4), bytes: Some((2048, true)), problem: Some(Problem::NeedsAccess(home)) }));
        assert!(partly.action.is_some() && partly.settings);
        assert!(
            partly.body.starts_with(&format!(
                "The 4 items Arbor can read in the {BIN} (2.0 KB) will be deleted permanently. This can't be undone."
            )),
            "{}",
            partly.body
        );
        assert!(partly.body.contains("“/Users/me/.Trash” can't be read") && partly.body.contains("Full Disk Access"), "{}", partly.body);
    }

    /// A trash folder whose listing fails partway (after some entries) is left out whole, so
    /// the dialog's count and what's emptied are only the trashes read to the end, and its
    /// "… can't be read, so what's in it stays" is true of everything in it.
    #[test]
    fn a_trash_whose_listing_fails_partway_is_left_out_whole() {
        let d = std::env::temp_dir().join(format!("arbor-trash-partway-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let (good, bad) = (d.join("good/.Trash-1000"), d.join("bad/.Trash-1000"));
        for (root, names) in [(&good, &["a", "b"][..]), (&bad, &["x", "y", "z"][..])] {
            fs::create_dir_all(root.join("files")).unwrap();
            fs::create_dir_all(root.join("info")).unwrap();
            for n in names {
                fs::write(root.join("files").join(n), "x").unwrap();
            }
        }
        *FAIL_AFTER.lock().unwrap() = Some((bad.join("files"), 1));
        let bins = [Bin::freedesktop(&good, me()), Bin::freedesktop(&bad, me())];
        let (items, problem) = list(&bins, &|| false).unwrap();
        *FAIL_AFTER.lock().unwrap() = None;
        let mut names: Vec<String> = items.iter().map(|i| i.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["a", "b"], "nothing from the trash whose listing failed");
        let p = problem.expect("the failure is the problem");
        let words = ask_words(Some(&Summary { items: None, count: Some(2), bytes: None, problem: Some(p) }));
        assert!(words.body.contains("The 2 items Arbor can read") && words.body.contains("so what's in it stays"), "{}", words.body);
        assert!(words.body.contains("bad"), "names the trash left out: {}", words.body);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_refusal_on_macos_means_full_disk_access() {
        let eperm = io::Error::from(io::ErrorKind::PermissionDenied);
        assert_eq!(problem_of(Path::new("/x/.Trash"), &eperm, true), Problem::NeedsAccess("/x/.Trash".into()));
        assert!(matches!(problem_of(Path::new("/x/.Trash"), &eperm, false), Problem::Unreadable(_)));
        let gone = io::Error::from(io::ErrorKind::Other);
        assert!(matches!(problem_of(Path::new("/x"), &gone, true), Problem::Unreadable(_)));
    }

    /// Windows: the result counts the Recycle Bin again afterwards and says what's still there.
    #[test]
    fn the_recycle_bins_result_is_counted_again() {
        assert!(shell_note(Some((3, 2048)), 0, Some((0, 0))).unwrap().contains("deleted 3 items (2.0 KB) permanently"));
        assert!(shell_note(None, 0, Some((0, 0))).unwrap().starts_with("Emptied"));
        let part = shell_note(Some((5, 1)), 0x8000_4005u32 as i32, Some((2, 1))).unwrap_err();
        assert!(part.contains("deleted 3 items, 2 items still in it"), "{part}");
        assert!(shell_note(None, -1, None).is_err());
        assert!(shell_note(Some((1, 1)), 0, Some((1, 1))).unwrap_err().contains("1 item still in it"));
    }

    /// A record's `Path=` is read from its group, URL-decoded, and a relative one is from the
    /// disk's top folder; what can't be trusted is refused, not guessed.
    #[test]
    fn a_record_says_where_an_item_came_from() {
        // An absolute path on this platform (the records are Linux's, but the code is portable).
        let root = if cfg!(windows) { "C:/" } else { "/" };
        let top = PathBuf::from(format!("{root}media/usb"));
        let top = top.as_path();
        let info = |p: &str| format!("[Trash Info]\nPath={p}\nDeletionDate=2026-11-09T10:00:00\n");
        assert_eq!(
            parse_info(&info(&format!("{root}home/u/My%20Docs/a.txt")), None).unwrap(),
            PathBuf::from(format!("{root}home/u/My Docs/a.txt"))
        );
        assert_eq!(parse_info(&info("docs/b%25.txt"), Some(top)).unwrap(), top.join("docs/b%.txt"));
        assert!(parse_info(&info("docs/b.txt"), None).is_err(), "relative, and no top folder to start from");
        assert!(parse_info(&info(&format!("{root}home/u/../../etc/passwd")), None).is_err());
        assert!(parse_info(&format!("[Other]\nPath={root}a\n"), None).is_err(), "only the Trash Info group counts");
        assert!(parse_info("[Trash Info]\nDeletionDate=x\n", None).is_err());
        assert_eq!(
            parse_info(&format!("[Other]\nPath={root}x\n[Trash Info]\r\nPath={root}y%2\r\n"), None).unwrap(),
            PathBuf::from(format!("{root}y%2"))
        );
    }

    #[test]
    fn trashes_are_known_by_their_paths() {
        let home = Path::new("/home/u/.local/share/Trash");
        assert_eq!(trash_of(&home.join("files"), Some(home)), Some(TrashDir { root: home.into(), top: None }));
        let usb = Path::new("/media/usb");
        assert_eq!(trash_of(&usb.join(".Trash-1000/files"), Some(home)).unwrap().top.as_deref(), Some(usb));
        assert_eq!(trash_of(&usb.join(".Trash/1000/files"), Some(home)).unwrap().top.as_deref(), Some(usb));
        assert_eq!(trash_of(&usb.join(".Trash-1000/info"), Some(home)), None);
        assert_eq!(trash_of(&usb.join(".Trash-x/files"), Some(home)), None);
        assert_eq!(trash_of(Path::new("/home/u/files"), Some(home)), None);
    }
}
