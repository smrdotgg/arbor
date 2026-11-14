//! Get Info (⌘I): a panel, where the quick preview goes, with what the disk says about the
//! selected item: name, kind, where it is (copyable), size, created, modified and accessed,
//! permissions and owner (Unix) or attributes (Windows), where a link points, and for a folder
//! (or several items) the total size and item count.
//!
//! The total is view-triggered background work and follows NORTHSTAR's rules for it:
//!
//! - **First paint never waits:** the panel shows at once with what the listing knows (name,
//!   kind, size, modified); the item's own details land from a worker a moment later (the key
//!   never waits for them), and the total streams in.
//! - **Scoped to what's on screen:** moving the selection or closing the panel sets the walk's
//!   stop flag, checked before every entry; its later counts never land.
//! - **Finishes and stops:** one walk, no watching. It never follows links, never enters
//!   another file system mounted inside (the device, and on macOS the volume, as Delete
//!   Permanently tells them apart), and counts what it couldn't read instead of guessing.
//! - **Nothing stored:** come back and it walks again.
//! - **A slow disk slows the count, never the window:** the walk has a thread of its own, and
//!   the window looks at its running count at most once a frame (the walk wakes it at most
//!   every 30 ms).
//!
//! Rename is offered (the list's own inline rename, undoable). Toggling attributes isn't: it
//! couldn't go on the undo stack (DECISIONS.md).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rune::Latest;

use crate::app::State;
use crate::fs;
use crate::mounts::{Fence, Mounts};

/// How often a walk wakes the window at most while it counts.
const WAKE_EVERY: Duration = Duration::from_millis(30);

/// What the disk says about one item.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    pub kind: String,
    pub size: u64,
    pub is_dir: bool,
    /// Milliseconds since the Unix epoch, when the platform says.
    pub created: Option<i64>,
    pub modified: Option<i64>,
    pub accessed: Option<i64>,
    /// Unix: `drwxr-xr-x (755)`.
    pub mode: Option<String>,
    /// Unix: `smr:staff`.
    pub owner: Option<String>,
    /// Windows: Read-only, Hidden, System, Archive.
    pub attrs: Option<String>,
    /// A link's target.
    pub link: Option<String>,
}

/// A walk's running count: what it has found so far, and whether it's finished. `files` and
/// `folders` are what's inside the folders walked, never the selected items themselves (a
/// folder of 2,000 files is 2,000 items, as Finder and Explorer say); `bytes` adds the selected
/// files' own sizes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Total {
    pub bytes: u64,
    pub files: u64,
    pub folders: u64,
    /// Folders it couldn't read (no permission, gone meanwhile).
    pub unreadable: u64,
    /// Other file systems mounted inside, not entered.
    pub mounts: u64,
    /// How many items are selected when it's several (0 for one folder, or the folder on
    /// screen), and how many of those are folders that are walked.
    pub selected: u64,
    pub selected_dirs: u64,
    pub done: bool,
}

impl Total {
    /// One folder: "1,234 items · 4.5 GB", or "Empty". Several items: "3 selected · 2,014 items
    /// inside · 4.5 GB" ("3 selected · 62 B" when none is a folder). With "so far" while it
    /// counts.
    pub fn line(&self) -> String {
        let items = self.files + self.folders;
        let n = |k: u64| format!("{} item{}", group(k), if k == 1 { "" } else { "s" });
        let size = fs::fmt_size(self.bytes);
        let mut s = if self.selected > 1 {
            if self.selected_dirs == 0 {
                format!("{} selected · {size}", group(self.selected))
            } else {
                format!("{} selected · {} inside · {size}", group(self.selected), n(items))
            }
        } else if items == 0 && self.done && self.unreadable == 0 && self.mounts == 0 {
            "Empty".into()
        } else {
            format!("{} · {size}", n(items))
        };
        if !self.done {
            s.push_str(" so far…");
        }
        s
    }

    /// What it left out, if anything.
    pub fn caveats(&self) -> Option<String> {
        let mut v = Vec::new();
        if self.unreadable > 0 {
            v.push(format!("{} folder{} couldn't be read", group(self.unreadable), if self.unreadable == 1 { "" } else { "s" }));
        }
        if self.mounts > 0 {
            v.push(format!("{} mounted disk{} inside not counted", self.mounts, if self.mounts == 1 { "" } else { "s" }));
        }
        (!v.is_empty()).then(|| v.join(" · "))
    }
}

/// 12345 → "12,345".
pub fn group(n: u64) -> String {
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

/// A walk's count, shared with the window.
pub type Shared = Arc<Mutex<Total>>;

/// Where a walk starts, and what it must still be when it's opened.
#[derive(Clone, Debug)]
pub enum Root {
    /// A selected folder: opened without following a link, and only while it's still the item
    /// the listing showed (its identity), so one replaced by a link since is never followed.
    Picked(PathBuf, Option<fs::Ident>),
    /// The folder on screen, reached as the user reached it (a link in its path, or the path
    /// itself, is how they navigated there), and only while it's still the folder listed.
    Here(PathBuf, Option<crate::noclobber::DirId>),
}

impl Root {
    pub fn path(&self) -> &Path {
        match self {
            Root::Picked(p, _) | Root::Here(p, _) => p,
        }
    }
}

/// Adds up what's inside the folders `roots` (not the folders themselves), never following
/// links or entering another file system, until done or `stop` is set. `seed` is already
/// counted (the selected files' sizes, which the listing had). Publishes into `out` as it goes, calling `wake`
/// at most every [`WAKE_EVERY`].
pub fn walk(roots: &[Root], seed: Total, out: &Shared, stop: &AtomicBool, wake: &dyn Fn()) {
    walk_with(roots, seed, out, stop, wake, &Mounts::read());
}

/// [`walk`], with the mount points to stop at (besides another device).
fn walk_with(roots: &[Root], seed: Total, out: &Shared, stop: &AtomicBool, wake: &dyn Fn(), mounts: &Mounts) {
    let mut c = Counting { t: seed, n: 0, last: Instant::now(), out, wake };
    for root in roots {
        let fence = Fence::below(root.path(), mounts);
        if !walk_tree(root, &fence, &mut c, stop) {
            return;
        }
    }
    c.t.done = true;
    c.publish();
}

/// A walk's running count, published at most every [`WAKE_EVERY`].
struct Counting<'a> {
    t: Total,
    n: u32,
    last: Instant,
    out: &'a Shared,
    wake: &'a dyn Fn(),
}

impl Counting<'_> {
    fn publish(&self) {
        if let Ok(mut o) = self.out.lock() {
            *o = self.t.clone();
        }
        (self.wake)();
    }

    /// One more entry counted.
    fn tick(&mut self) {
        self.n += 1;
        if self.n.is_multiple_of(64) && self.last.elapsed() >= WAKE_EVERY {
            self.last = Instant::now();
            self.publish();
        }
    }
}

/// Counts everything under folder `root` (see [`Root`] for how it's opened: a selected folder
/// without following a link, the folder on screen as the user reached it; either only while
/// it's still the one listed). Below it nothing is followed: each folder is opened without
/// following a link and only if it's still the folder that was listed, and its entries are
/// `lstat`ed through it, so a folder swapped for a link mid-walk is never entered. A folder
/// whose entries can't be described (readable but not searchable) counts as one that couldn't
/// be read. False when stopped.
#[cfg(unix)]
fn walk_tree(root: &Root, fence: &Fence, c: &mut Counting, stop: &AtomicBool) -> bool {
    use crate::dirfd::{Names, fstat, is_dir, open_dir, same, stat_at};
    use std::os::unix::ffi::OsStrExt;
    let (path, follow) = match root {
        Root::Picked(p, _) => (p, false),
        Root::Here(p, _) => (p, true),
    };
    let opened = crate::ffi::cstr(path).and_then(|r| open_dir(libc::AT_FDCWD, &r, follow)).and_then(|fd| Ok((fstat(&fd)?, fd)));
    // A selected folder: the object listed, birth time and all (a folder made where it was,
    // that got its inode back, isn't it). The folder on screen: the one listed, links followed.
    #[allow(clippy::unnecessary_cast)] // `dev_t` is i32 on macOS
    let still = |st: &libc::stat, fd: &std::os::fd::OwnedFd| match root {
        Root::Picked(_, id) => id.is_none_or(|id| crate::dirfd::ident_of_fd(fd).is_ok_and(|now| now == id)),
        Root::Here(_, id) => id.is_none_or(|w| w.file_id() == (st.st_dev as u64, st.st_ino as u64)),
    };
    let (st, fd) = match opened {
        Ok((st, fd)) if still(&st, &fd) => (st, fd),
        _ => {
            c.t.unreadable += 1;
            return true;
        }
    };
    let mut stack = vec![(path.to_path_buf(), st, Some(fd))];
    while let Some((dir, want, fd)) = stack.pop() {
        if stop.load(Relaxed) {
            return false;
        }
        #[cfg(test)]
        hook::at(&dir);
        let fd = match fd {
            Some(fd) => Ok(fd),
            None => crate::ffi::cstr(&dir).and_then(|d| open_dir(libc::AT_FDCWD, &d, false)),
        };
        let fd = match fd.and_then(|fd| Ok((fstat(&fd)?, fd))) {
            Ok((now, fd)) if same(&now, &want) => fd,
            _ => {
                c.t.unreadable += 1;
                continue;
            }
        };
        // One name at a time: a stop between two reads reads nothing more, and a read error
        // counts the folder as unreadable rather than ending it quietly.
        let Ok(mut entries) = Names::open(&fd) else {
            c.t.unreadable += 1;
            continue;
        };
        let mut denied = false;
        loop {
            if stop.load(Relaxed) {
                return false;
            }
            let n = match entries.next() {
                None => break,
                Some(Ok(n)) => n,
                Some(Err(_)) => {
                    denied = true;
                    break;
                }
            };
            match stat_at(&fd, &n) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => denied = true,
                Ok(st) if is_dir(&st) => {
                    let p = dir.join(std::ffi::OsStr::from_bytes(n.as_bytes()));
                    // As `MetadataExt::dev` gives it (`dev_t` is i32 on macOS).
                    #[allow(clippy::unnecessary_cast)]
                    let dev = st.st_dev as u64;
                    if fence.crosses_dev(&p, Some(dev), true) {
                        c.t.mounts += 1;
                        continue;
                    }
                    c.t.folders += 1;
                    stack.push((p, st, None));
                }
                Ok(st) => {
                    c.t.files += 1;
                    c.t.bytes += st.st_size as u64;
                }
            }
            c.tick();
        }
        if denied {
            c.t.unreadable += 1;
        }
    }
    true
}

/// [`walk_tree`] where there are no descriptor-relative calls (Windows): each folder is
/// checked right before it's read to be a folder, not a link or junction, with the identity it
/// was listed with.
#[cfg(not(unix))]
fn walk_tree(root: &Root, fence: &Fence, c: &mut Counting, stop: &AtomicBool) -> bool {
    let still = match root {
        Root::Picked(p, id) => {
            p.symlink_metadata().is_ok_and(|m| m.is_dir() && !m.is_symlink() && id.is_none_or(|id| fs::ident_of(p, &m) == Some(id)))
        }
        Root::Here(p, id) => id.is_none_or(|id| crate::noclobber::dir_id(p) == Some(id)),
    };
    if !still {
        c.t.unreadable += 1;
        return true;
    }
    let mut stack: Vec<(PathBuf, Option<fs::Ident>)> = vec![(root.path().to_path_buf(), None)];
    while let Some((dir, want)) = stack.pop() {
        if stop.load(Relaxed) {
            return false;
        }
        #[cfg(test)]
        hook::at(&dir);
        // The same folder (its object identity, with its volume and file id: a creation time
        // alone can be a predecessor's; its times changing as things in it do don't touch it).
        if want.is_some() && !dir.symlink_metadata().is_ok_and(|m| m.is_dir() && !m.is_symlink() && fs::ident_of(&dir, &m) == want) {
            c.t.unreadable += 1;
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            c.t.unreadable += 1;
            continue;
        };
        let mut denied = false;
        for de in rd {
            if stop.load(Relaxed) {
                return false;
            }
            let Ok(de) = de else {
                denied = true;
                break;
            };
            let p = de.path();
            // An lstat: links are counted as themselves, never followed.
            match p.symlink_metadata() {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => denied = true,
                Ok(m) if m.is_dir() => {
                    if fence.crosses(&p, &m) {
                        c.t.mounts += 1;
                        continue;
                    }
                    c.t.folders += 1;
                    let id = fs::ident_of(&p, &m);
                    stack.push((p, id));
                }
                Ok(m) => {
                    c.t.files += 1;
                    c.t.bytes += m.len();
                }
            }
            c.tick();
        }
        if denied {
            c.t.unreadable += 1;
        }
    }
    true
}

/// Test only: holds a walk as it reaches a folder, until told.
#[cfg(test)]
pub(crate) mod hook {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, Sender};

    /// A folder, told when the walk reaches it, then waiting to be told to go on.
    type Gate = (PathBuf, Sender<()>, Receiver<()>);

    pub(crate) static GATES: Mutex<Vec<Gate>> = Mutex::new(Vec::new());

    /// Paths whose details take this long to read (a slow disk).
    pub(crate) static SLOW: Mutex<Vec<(PathBuf, std::time::Duration)>> = Mutex::new(Vec::new());

    /// The threads `meta` ran on: never the window's.
    pub(crate) static META_THREADS: Mutex<Vec<std::thread::ThreadId>> = Mutex::new(Vec::new());

    pub(super) fn slow_meta(p: &Path) {
        META_THREADS.lock().unwrap().push(std::thread::current().id());
        let wait = SLOW.lock().unwrap().iter().find(|(q, _)| q == p).map(|(_, w)| *w);
        if let Some(w) = wait {
            std::thread::sleep(w);
        }
    }

    pub(super) fn at(p: &Path) {
        let gate = {
            let mut g = GATES.lock().unwrap();
            g.iter().position(|(q, ..)| q == p).map(|i| g.remove(i))
        };
        if let Some((_, reached, go)) = gate {
            let _ = reached.send(());
            let _ = go.recv();
        }
    }
}

/// Reads what the disk says about `p` (an `lstat`, the owner's names, a link's target). On a
/// worker.
pub fn meta(p: &Path) -> Result<Meta, String> {
    #[cfg(test)]
    hook::slow_meta(p);
    let m = p.symlink_metadata().map_err(|e| e.to_string())?;
    let ms = |t: std::io::Result<std::time::SystemTime>| {
        t.ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64)
    };
    let is_link = m.file_type().is_symlink();
    let target_dir = is_link && std::fs::metadata(p).is_ok_and(|t| t.is_dir());
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let kind = if is_link {
        if target_dir { "Link to a folder".to_string() } else { "Link".to_string() }
    } else if m.is_dir() {
        "Folder".into()
    } else {
        kind_name(&name)
    };
    let mut out = Meta {
        kind,
        size: if m.is_dir() { 0 } else { m.len() },
        is_dir: m.is_dir(),
        created: ms(m.created()),
        modified: ms(m.modified()),
        accessed: ms(m.accessed()),
        link: is_link.then(|| std::fs::read_link(p).map_or_else(|e| format!("(couldn't read it: {e})"), |t| t.display().to_string())),
        ..Meta::default()
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        out.mode = Some(mode_string(m.mode()));
        out.owner = Some(format!("{}:{}", user(m.uid()), group_name(m.gid())));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        out.attrs = Some(attr_string(m.file_attributes()));
    }
    Ok(out)
}

/// "PNG file", "File".
pub fn kind_name(name: &str) -> String {
    match fs::ext(&name.to_lowercase()) {
        "" => "File".into(),
        x => format!("{} file", x.to_uppercase()),
    }
}

/// A Unix mode as `ls -l` writes it, with its octal permissions: `-rw-r--r-- (644)`.
#[cfg_attr(not(any(unix, test)), allow(dead_code))]
pub fn mode_string(mode: u32) -> String {
    let kind = match mode & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        0o010000 => 'p',
        0o140000 => 's',
        0o020000 => 'c',
        0o060000 => 'b',
        _ => '-',
    };
    let mut s = String::from(kind);
    let bit = |m: u32, c: char| if mode & m != 0 { c } else { '-' };
    let special = |x: u32, sp: u32, on: char, off: char| match (mode & x != 0, mode & sp != 0) {
        (true, true) => on,
        (false, true) => off,
        (true, false) => 'x',
        (false, false) => '-',
    };
    s.push(bit(0o400, 'r'));
    s.push(bit(0o200, 'w'));
    s.push(special(0o100, 0o4000, 's', 'S'));
    s.push(bit(0o040, 'r'));
    s.push(bit(0o020, 'w'));
    s.push(special(0o010, 0o2000, 's', 'S'));
    s.push(bit(0o004, 'r'));
    s.push(bit(0o002, 'w'));
    s.push(special(0o001, 0o1000, 't', 'T'));
    format!("{s} ({:o})", mode & 0o7777)
}

/// Windows attributes in words: "Read-only, Hidden", or "Normal".
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub fn attr_string(a: u32) -> String {
    let names = [
        (0x1, "Read-only"),
        (0x2, "Hidden"),
        (0x4, "System"),
        (0x20, "Archive"),
        (0x400, "Reparse point"),
        (0x800, "Compressed"),
        (0x4000, "Encrypted"),
    ];
    let v: Vec<&str> = names.iter().filter(|(b, _)| a & b != 0).map(|(_, n)| *n).collect();
    if v.is_empty() { "Normal".into() } else { v.join(", ") }
}

#[cfg(unix)]
fn user(uid: u32) -> String {
    let mut buf = vec![0 as libc::c_char; 4096];
    // SAFETY: a zeroed passwd and a buffer of the size given; the result points into them.
    unsafe {
        let mut pw: libc::passwd = std::mem::zeroed();
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        if libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) == 0 && !out.is_null() && !pw.pw_name.is_null() {
            return std::ffi::CStr::from_ptr(pw.pw_name).to_string_lossy().into_owned();
        }
    }
    uid.to_string()
}

#[cfg(unix)]
fn group_name(gid: u32) -> String {
    let mut buf = vec![0 as libc::c_char; 4096];
    // SAFETY: as `user`, for a group.
    unsafe {
        let mut gr: libc::group = std::mem::zeroed();
        let mut out: *mut libc::group = std::ptr::null_mut();
        if libc::getgrgid_r(gid, &mut gr, buf.as_mut_ptr(), buf.len(), &mut out) == 0 && !out.is_null() && !gr.gr_name.is_null() {
            return std::ffi::CStr::from_ptr(gr.gr_name).to_string_lossy().into_owned();
        }
    }
    gid.to_string()
}

/// A full date: "9 Nov 2026, 14:03".
pub fn fmt_date(ms: Option<i64>) -> String {
    let Some(ms) = ms.filter(|&m| m != 0) else { return "—".into() };
    const MON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let t = rune::LocalTime::at(ms);
    format!("{} {} {}, {:02}:{:02}", t.day, MON[(t.month as usize).saturating_sub(1).min(11)], t.year, t.hour, t.minute)
}

/// The panel while it's open.
pub struct Info {
    /// The pane, folder and selection it shows (to follow the selection without allocating).
    key: (u64, u64, u64, usize, usize),
    dir: PathBuf,
    /// The items, from the listing.
    pub paths: Vec<PathBuf>,
    pub name: String,
    pub dirs: usize,
    pub files: usize,
    /// The listing's size of a single file.
    pub size: u64,
    /// One item's details, once read (`None` while on its way; always `None` for several).
    pub meta: Option<Result<Meta, String>>,
    /// What the listing already says about the one item (kind, size, modified), shown until
    /// its details land.
    pub known: Option<Meta>,
    req: u64,
    /// The total, for a folder or several items: what the walk has counted so far.
    pub total: Option<Shared>,
    stop: Arc<AtomicBool>,
}

impl Drop for Info {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

/// Walks started, for tests.
pub static WALKS: AtomicU64 = AtomicU64::new(0);

/// The worker reading one item's details.
pub type MetaLoader = Latest<(u64, Result<Meta, String>)>;

impl State {
    /// ⌘I: opens Get Info on the selection (closing the preview, which it replaces), or
    /// closes it.
    pub fn toggle_info(&mut self) {
        if self.info.is_some() {
            self.close_info();
            return;
        }
        self.close_preview();
        self.retarget_info();
    }

    pub fn close_info(&mut self) {
        self.info = None;
    }

    fn info_key(&self) -> (u64, u64, u64, usize, usize) {
        let f = self.cur();
        (f.id, f.version, f.picks, f.sel, f.marked.len())
    }

    /// Keeps the panel on the active pane's selection (called with `sync_preview`). Open, it's
    /// a comparison of five numbers; closed, a `None` check. True when it changed.
    pub(crate) fn sync_info(&mut self) -> bool {
        let Some(i) = &self.info else { return false };
        if i.key == self.info_key() && i.dir == self.cur().cwd {
            return false;
        }
        self.retarget_info();
        true
    }

    /// Points the panel at the selection (the folder on screen when nothing is selected),
    /// stopping the last walk and starting what this one needs.
    fn retarget_info(&mut self) {
        // The last walk stops before anything else happens.
        if let Some(old) = &self.info {
            old.stop.store(true, Relaxed);
        }
        let key = self.info_key();
        let f = self.cur();
        // An item showing a label isn't read until the folder is read again: the folder is
        // shown meanwhile.
        let picked = if f.updating() { Vec::new() } else { f.targets() };
        let (paths, dirs, files, size, name) = if picked.is_empty() {
            (vec![f.cwd.clone()], 1, 0, 0, crate::sidebar::name_for(&f.cwd))
        } else {
            let dirs = picked.iter().filter(|e| e.is_dir && !e.is_link).count();
            let size = picked.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
            let name = if picked.len() == 1 { picked[0].name.clone() } else { format!("{} items", picked.len()) };
            (picked.iter().map(|e| f.cwd.join(&e.os)).collect(), dirs, picked.len() - dirs, size, name)
        };
        let dir = f.cwd.clone();
        // Folders (not links to them) are walked; selected files count from the listing.
        let walk_roots: Vec<Root> = if picked.is_empty() {
            vec![Root::Here(f.cwd.clone(), f.dir_id)]
        } else {
            picked.iter().filter(|e| e.is_dir && !e.is_link).map(|e| Root::Picked(f.cwd.join(&e.os), e.id)).collect()
        };
        let known = match picked.as_slice() {
            [e] => Some(Meta {
                kind: if e.is_link {
                    "Link".into()
                } else if e.is_dir {
                    "Folder".into()
                } else {
                    kind_name(&e.name)
                },
                size: e.size,
                is_dir: e.is_dir && !e.is_link,
                modified: (e.mtime_ms != 0).then_some(e.mtime_ms),
                ..Meta::default()
            }),
            [] => Some(Meta { kind: "Folder".into(), is_dir: true, ..Meta::default() }),
            _ => None,
        };
        let seed = if paths.len() > 1 {
            Total { bytes: size, selected: paths.len() as u64, selected_dirs: walk_roots.len() as u64, ..Total::default() }
        } else {
            Total::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        self.next_info += 1;
        let req = self.next_info;
        // The item's details come from a worker; the key never waits for them (a local disk
        // answers within the next frame, a slow one when it can).
        if paths.len() == 1 {
            self.meta_loader.request((req, paths[0].clone()), |(req, p)| (req, meta(&p)));
        } else {
            self.meta_loader.cancel();
        }
        let total = (!walk_roots.is_empty() || paths.len() > 1).then(|| {
            let shared: Shared = Arc::new(Mutex::new(seed.clone()));
            let (out, stop, waker) = (shared.clone(), stop.clone(), self.waker.clone());
            WALKS.fetch_add(1, Relaxed);
            std::thread::Builder::new()
                .name("satchel-info".into())
                .spawn(move || {
                    let wake = || {
                        if let Some(w) = &waker {
                            w();
                        }
                    };
                    walk(&walk_roots, seed, &out, &stop, &wake);
                })
                .expect("can't start the Get Info thread");
            shared
        });
        // Replacing the old panel drops it, which stops its walk.
        self.info = Some(Info { key, dir, paths, name, dirs, files, size, meta: None, known, req, total, stop });
    }

    /// An item's details that landed after the panel's wait, from `poll`.
    pub(crate) fn poll_info(&mut self) -> bool {
        let Some((req, m)) = self.meta_loader.take() else { return self.info.as_ref().is_some_and(|i| i.total.is_some()) };
        match self.info.as_mut() {
            Some(i) if i.req == req => {
                i.meta = Some(m);
                true
            }
            _ => false,
        }
    }

    /// "Copy" beside the path: the item's full path as text.
    pub fn info_copy_path(&mut self, tx: &mut rune::Tx) {
        let Some(i) = &self.info else { return };
        let text = i.paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
        self.note = Some(format!("Copied {text}"));
        tx.copy(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{settle_all, state_in};

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("satchel-info-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn here(roots: &[PathBuf]) -> Vec<Root> {
        roots.iter().map(|p| Root::Here(p.clone(), None)).collect()
    }

    fn total_of(roots: &[PathBuf]) -> Total {
        let out: Shared = Arc::default();
        walk(&here(roots), Total::default(), &out, &AtomicBool::new(false), &|| {});
        out.lock().unwrap().clone()
    }

    #[test]
    fn a_folder_total_counts_everything_inside_once() {
        let d = temp("total");
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("x"), vec![0u8; 1000]).unwrap();
        std::fs::write(d.join("a/y"), vec![0u8; 500]).unwrap();
        std::fs::write(d.join("a/b/z"), vec![0u8; 25]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&d, d.join("a/loop")).unwrap();
        let t = total_of(std::slice::from_ref(&d));
        let links = u64::from(cfg!(unix));
        assert_eq!((t.folders, t.files, t.done), (2, 3 + links, true), "{t:?}");
        assert!(t.bytes >= 1525 && t.bytes < 1525 + 4096, "a link counts as itself, never followed: {t:?}");
        assert_eq!(t.line(), format!("{} items · {}", 5 + links, fs::fmt_size(t.bytes)));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A folder it can't read is counted as such, and the walk goes on.
    #[cfg(unix)]
    #[test]
    fn unreadable_folders_are_counted_not_guessed() {
        use std::os::unix::fs::PermissionsExt;
        let d = temp("perm");
        std::fs::create_dir_all(d.join("locked")).unwrap();
        std::fs::write(d.join("locked/secret"), "x").unwrap();
        std::fs::write(d.join("open"), "xy").unwrap();
        std::fs::set_permissions(d.join("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let t = total_of(std::slice::from_ref(&d));
        std::fs::set_permissions(d.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
        // Root can read anything (CI containers): then it's counted.
        if t.unreadable == 1 {
            assert_eq!((t.files, t.folders), (1, 1));
            assert_eq!(t.caveats().as_deref(), Some("1 folder couldn't be read"));
        } else {
            assert_eq!(t.files, 2);
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A file system mounted inside isn't entered or counted, only noted (a bind mount on the
    /// same device too: the mount table says so).
    #[test]
    fn a_walk_never_crosses_into_another_mount() {
        let d = temp("mount");
        std::fs::create_dir_all(d.join("disk/deep")).unwrap();
        std::fs::write(d.join("disk/deep/big"), vec![0u8; 5000]).unwrap();
        std::fs::write(d.join("mine"), "abc").unwrap();
        let out: Shared = Arc::default();
        let canon = std::fs::canonicalize(&d).unwrap();
        walk_with(
            &here(std::slice::from_ref(&d)),
            Total::default(),
            &out,
            &AtomicBool::new(false),
            &|| {},
            &Mounts::of(&[&canon.join("disk")]),
        );
        let t = out.lock().unwrap().clone();
        assert_eq!((t.bytes, t.files, t.folders, t.mounts, t.done), (3, 1, 0, 1, true), "{t:?}");
        assert_eq!(t.caveats().as_deref(), Some("1 mounted disk inside not counted"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Get Info on a mount point itself (`/`, a volume's top folder): what's on it counts; only
    /// mounts below it are left out (it used to count one folder).
    #[test]
    fn a_mount_root_counts_what_is_on_it() {
        let d = temp("mount-root");
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::create_dir_all(d.join("disk")).unwrap();
        std::fs::write(d.join("a/f"), "abcd").unwrap();
        let out: Shared = Arc::default();
        let canon = std::fs::canonicalize(&d).unwrap();
        walk_with(
            &here(std::slice::from_ref(&d)),
            Total::default(),
            &out,
            &AtomicBool::new(false),
            &|| {},
            &Mounts::of(&[&canon, &canon.join("disk")]),
        );
        let t = out.lock().unwrap().clone();
        assert_eq!((t.folders, t.files, t.bytes, t.mounts), (2, 1, 4, 1), "{t:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A folder that can be listed but whose entries can't be looked at (read, not search,
    /// permission) is one that couldn't be read, not a quiet zero.
    #[cfg(unix)]
    #[test]
    fn a_folder_whose_entries_cant_be_described_is_said() {
        use std::os::unix::fs::PermissionsExt;
        let d = temp("unsearchable");
        let locked = d.join("list-only");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("secret"), "hidden").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o444)).unwrap();
        let root = locked.join("secret").symlink_metadata().is_ok();
        let t = total_of(std::slice::from_ref(&d));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        if root {
            return;
        }
        assert_eq!(t.unreadable, 1, "{t:?}");
        assert_eq!(t.caveats().as_deref(), Some("1 folder couldn't be read"));
    }

    /// A folder swapped for a link to somewhere else while the walk waits to enter it is never
    /// followed, whether it's the root or a folder below it.
    #[cfg(unix)]
    #[test]
    fn a_folder_swapped_for_a_link_mid_walk_isnt_followed() {
        for at_root in [false, true] {
            let d = temp(if at_root { "swap-root" } else { "swap-sub" });
            let root = d.join("root");
            let queued = if at_root { root.clone() } else { root.join("sub") };
            std::fs::create_dir_all(&queued).unwrap();
            std::fs::create_dir_all(d.join("other")).unwrap();
            std::fs::write(d.join("other/outside"), vec![0u8; 12345]).unwrap();
            let (reached_tx, reached) = std::sync::mpsc::channel();
            let (go, go_rx) = std::sync::mpsc::channel();
            hook::GATES.lock().unwrap().push((queued.clone(), reached_tx, go_rx));
            let out: Shared = Arc::default();
            let (o, r) = (out.clone(), root.clone());
            let walker = std::thread::spawn(move || walk(&here(&[r]), Total::default(), &o, &AtomicBool::new(false), &|| {}));
            reached.recv_timeout(Duration::from_secs(5)).unwrap();
            std::fs::rename(&queued, d.join("aside")).unwrap();
            std::os::unix::fs::symlink(d.join("other"), &queued).unwrap();
            go.send(()).unwrap();
            walker.join().unwrap();
            let t = out.lock().unwrap().clone();
            assert_eq!(t.bytes, 0, "at_root={at_root}: counted what's outside through the link: {t:?}");
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// The review's case: a stop while a big folder waits to be read used to read all of it
    /// first (the names were collected before the stop was looked at: 131 reads after a
    /// cancel). Now nothing more is read.
    #[cfg(unix)]
    #[test]
    fn a_stop_reads_nothing_more_of_a_folder() {
        let d = temp("stop-reads");
        let big = d.join("big");
        std::fs::create_dir_all(&big).unwrap();
        for i in 0..300 {
            std::fs::write(big.join(format!("f{i}")), "x").unwrap();
        }
        let ino = crate::dirfd::hook::watch(&big, None);
        let (reached_tx, reached) = std::sync::mpsc::channel();
        let (go, go_rx) = std::sync::mpsc::channel();
        hook::GATES.lock().unwrap().push((big.clone(), reached_tx, go_rx));
        let (out, stop): (Shared, Arc<AtomicBool>) = (Arc::default(), Arc::default());
        let (o, st, r) = (out.clone(), stop.clone(), d.clone());
        let walker = std::thread::spawn(move || walk(&here(&[r]), Total::default(), &o, &st, &|| {}));
        reached.recv_timeout(Duration::from_secs(5)).unwrap();
        stop.store(true, Relaxed);
        go.send(()).unwrap();
        walker.join().unwrap();
        assert_eq!(crate::dirfd::hook::reads(ino), 0, "read after the stop");
        assert!(!out.lock().unwrap().done);
        crate::dirfd::hook::WATCH.lock().unwrap().retain(|w| w.0 != ino);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The review's case: a read error partway through a folder (an injected `EIO`) used to
    /// end it quietly, a finished total with nothing unreadable. Now the folder counts as one
    /// that couldn't be read.
    #[cfg(unix)]
    #[test]
    fn a_read_error_is_unreadable_not_the_end() {
        let d = temp("eio");
        for i in 0..10 {
            std::fs::write(d.join(format!("f{i}")), "x").unwrap();
        }
        let ino = crate::dirfd::hook::watch(&d, Some(4));
        let t = total_of(std::slice::from_ref(&d));
        crate::dirfd::hook::WATCH.lock().unwrap().retain(|w| w.0 != ino);
        assert!(t.done && t.unreadable == 1 && t.files < 10, "{t:?}");
        assert_eq!(t.caveats().as_deref(), Some("1 folder couldn't be read"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The review's case: a selected folder replaced by a link after it was listed used to be
    /// followed (12,345 bytes from outside counted). It's opened without following a link and
    /// only while it's the item listed, so it counts as unreadable. The folder on screen,
    /// The review's case: a selected folder replaced by another made at its path, which got its
    /// inode back (ext4), was counted: the walk compared device and inode only. Its birth time
    /// (or, without one, its handle or ctime) tells them apart.
    #[cfg(unix)]
    #[test]
    fn a_selected_folder_whose_inode_was_reused_isnt_counted() {
        use std::os::unix::fs::MetadataExt;
        let d = temp("dir-reuse");
        let p = d.join("folder");
        let mut shown = false;
        for _ in 0..20 {
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir(&p).unwrap();
            let (id, ino) = (crate::fs::ident_at(&p).unwrap(), p.symlink_metadata().unwrap().ino());
            std::thread::sleep(Duration::from_millis(20));
            std::fs::remove_dir(&p).unwrap();
            std::fs::create_dir(&p).unwrap();
            if p.symlink_metadata().unwrap().ino() != ino {
                continue;
            }
            shown = true;
            std::fs::write(p.join("only in the newcomer"), "x").unwrap();
            let total = std::sync::Arc::new(std::sync::Mutex::new(Total::default()));
            walk(&[Root::Picked(p.clone(), Some(id))], Total::default(), &total, &AtomicBool::new(false), &|| {});
            let t = total.lock().unwrap().clone();
            assert_eq!((t.files, t.unreadable), (0, 1), "{t:?}");
            // The folder chosen, with something put in it since: counted.
            let id = crate::fs::ident_at(&p).unwrap();
            std::fs::write(p.join("added since"), "y").unwrap();
            let total = std::sync::Arc::new(std::sync::Mutex::new(Total::default()));
            walk(&[Root::Picked(p.clone(), Some(id))], Total::default(), &total, &AtomicBool::new(false), &|| {});
            assert_eq!(total.lock().unwrap().files, 2);
            break;
        }
        if !shown {
            eprintln!("a_selected_folder_whose_inode_was_reused_isnt_counted: skipped (no inode reuse here)");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// reached through a link to it, is still counted (that's how the user got there).
    #[cfg(unix)]
    #[test]
    fn a_selected_folder_replaced_by_a_link_isnt_followed() {
        let d = temp("root-link");
        std::fs::create_dir_all(d.join("here/sub")).unwrap();
        // Hidden, so a folder showing it (hidden files hidden) has nothing selected.
        std::fs::write(d.join("here/sub/.mine"), "abc").unwrap();
        std::fs::create_dir_all(d.join("other")).unwrap();
        std::fs::write(d.join("other/outside"), vec![0u8; 12345]).unwrap();
        let mut s = state_in(&d.join("here"));
        settle_all(&mut s);
        let wait = |s: &State| {
            let total = s.info.as_ref().unwrap().total.clone().unwrap();
            let until = Instant::now() + Duration::from_secs(5);
            while !total.lock().unwrap().done && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(2));
            }
            total.lock().unwrap().clone()
        };
        s.cur_mut().select(0);
        std::fs::rename(d.join("here/sub"), d.join("aside")).unwrap();
        std::os::unix::fs::symlink(d.join("other"), d.join("here/sub")).unwrap();
        s.toggle_info();
        let t = wait(&s);
        assert_eq!((t.bytes, t.unreadable), (0, 1), "{t:?}");
        s.close_info();
        // On screen through a link: counted.
        std::os::unix::fs::symlink(d.join("aside"), d.join("alias")).unwrap();
        let mut s = state_in(&d.join("alias"));
        settle_all(&mut s);
        s.toggle_info();
        let t = wait(&s);
        assert_eq!((t.bytes, t.files, t.unreadable), (3, 1, 0), "{t:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The key that opens Get Info (or moves its selection) never reads the disk or waits for
    /// a worker: the panel shows at once with what the listing knows, and the item's details
    /// land through `poll`, even for a local file (the key used to wait up to 2 ms, which
    /// macOS stretched to 9.5 ms on CI), and from a slow disk whenever they come.
    #[test]
    fn the_key_never_waits_for_the_details() {
        let d = temp("slow-meta");
        std::fs::write(d.join("f.txt"), "x").unwrap();
        std::fs::write(d.join("g.txt"), "yy").unwrap();
        let mut s = state_in(&d);
        settle_all(&mut s);
        let here = std::thread::current().id();
        s.toggle_info();
        let i = s.info.as_ref().unwrap();
        assert!(i.meta.is_none(), "the key didn't wait for the details");
        assert_eq!(i.known.as_ref().map(|k| (k.kind.as_str(), k.size)), Some(("TXT file", 1)), "what the listing knows shows at once");
        let until = Instant::now() + Duration::from_secs(5);
        while s.info.as_ref().is_some_and(|i| i.meta.is_none()) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
            s.poll_info();
        }
        assert!(s.info.as_ref().is_some_and(|i| matches!(i.meta, Some(Ok(_)))), "and they land later");
        // A slow disk: moving the selection doesn't wait either.
        hook::SLOW.lock().unwrap().push((d.join("g.txt"), Duration::from_millis(300)));
        s.cur_mut().select(1);
        let t = Instant::now();
        s.sync_preview();
        assert!(t.elapsed() < Duration::from_millis(100), "the key waited {:?}", t.elapsed());
        assert!(s.info.as_ref().is_some_and(|i| i.meta.is_none() && i.known.as_ref().is_some_and(|k| k.size == 2)));
        assert!(!hook::META_THREADS.lock().unwrap().contains(&here), "the details were read on the window's thread");
    }

    /// On Linux, a real one: walking `/dev` never goes into `/dev/pts` and the like.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_walk_of_dev_stops_at_its_mounts() {
        let out: Shared = Arc::default();
        walk(&here(&[PathBuf::from("/dev")]), Total::default(), &out, &AtomicBool::new(false), &|| {});
        let t = out.lock().unwrap().clone();
        use std::os::unix::fs::MetadataExt;
        let other = |p: &str| matches!((std::fs::metadata(p), std::fs::metadata("/dev")), (Ok(a), Ok(b)) if a.dev() != b.dev());
        assert!(t.done && (t.mounts > 0 || !other("/dev/pts")), "{t:?}");
    }

    /// Closing the panel (or moving on) stops the walk at the next entry: nothing more is
    /// counted, and it never says it's done.
    #[test]
    fn closing_the_panel_stops_the_walk() {
        let d = temp("cancel");
        std::fs::create_dir_all(d.join("deep")).unwrap();
        for i in 0..50 {
            std::fs::write(d.join("deep").join(format!("f{i}")), "x").unwrap();
        }
        let mut s = state_in(&d);
        settle_all(&mut s);
        let i = s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].os == "deep").unwrap();
        s.cur_mut().select(i);
        let (reached_tx, reached) = std::sync::mpsc::channel();
        let (go, go_rx) = std::sync::mpsc::channel();
        hook::GATES.lock().unwrap().push((d.join("deep"), reached_tx, go_rx));
        s.toggle_info();
        let total = s.info.as_ref().unwrap().total.clone().unwrap();
        reached.recv_timeout(Duration::from_secs(5)).unwrap();
        s.close_info();
        go.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let t = total.lock().unwrap().clone();
        assert!(!t.done && t.files == 0, "stopped before counting: {t:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The panel follows the selection: a file's details land (size, mode), a folder's total
    /// streams to done, several items add up.
    #[test]
    fn info_follows_the_selection() {
        let d = temp("follow");
        std::fs::write(d.join("a.txt"), vec![b'x'; 300]).unwrap();
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/b"), vec![b'x'; 200]).unwrap();
        let mut s = state_in(&d);
        settle_all(&mut s);
        let at = |s: &State, n: &str| s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].os == n).unwrap();
        let i = at(&s, "a.txt");
        s.cur_mut().select(i);
        s.toggle_info();
        let until = Instant::now() + Duration::from_secs(5);
        while s.info.as_ref().unwrap().meta.is_none() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
            s.poll(&mut rune::Tx::default());
        }
        let m = s.info.as_ref().unwrap().meta.clone().unwrap().unwrap();
        assert_eq!((m.kind.as_str(), m.size), ("TXT file", 300));
        #[cfg(unix)]
        assert!(m.mode.as_deref().is_some_and(|m| m.starts_with("-rw")), "{m:?}");
        assert!(s.info.as_ref().unwrap().total.is_none(), "a single file isn't walked");
        let j = at(&s, "sub");
        s.cur_mut().select(j);
        s.sync_preview();
        let total = s.info.as_ref().unwrap().total.clone().unwrap();
        while !total.lock().unwrap().done && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(total.lock().unwrap().bytes, 200);
        s.cur_mut().select_all();
        s.sync_preview();
        let i = s.info.as_ref().unwrap();
        assert_eq!((i.name.as_str(), i.dirs, i.files), ("2 items", 1, 1));
        let total = i.total.clone().unwrap();
        while !total.lock().unwrap().done && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
        }
        let t = total.lock().unwrap().clone();
        assert_eq!((t.bytes, t.files, t.folders, t.selected, t.selected_dirs), (500, 1, 0, 2, 1));
        assert_eq!(t.line(), format!("2 selected · 1 item inside · {}", fs::fmt_size(500)));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The QA run's case: a selected folder of 2,000 files said "2,001 items" and an empty one
    /// "1 item" (the folder counted itself). One folder shows what's in it, as Finder and
    /// Explorer do; several items say what's selected and what's inside them, apart.
    #[test]
    fn a_folder_counts_its_contents_not_itself() {
        let d = temp("contents");
        std::fs::create_dir_all(d.join("big")).unwrap();
        std::fs::create_dir_all(d.join("empty")).unwrap();
        for i in 0..2000 {
            std::fs::write(d.join("big").join(format!("f{i}")), "x").unwrap();
        }
        std::fs::write(d.join("loose.txt"), "abc").unwrap();
        let mut s = state_in(&d);
        settle_all(&mut s);
        let at = |s: &State, n: &str| s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].os == n).unwrap();
        let line = |s: &State| {
            let total = s.info.as_ref().unwrap().total.clone().unwrap();
            let until = Instant::now() + Duration::from_secs(10);
            while !total.lock().unwrap().done && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(2));
            }
            total.lock().unwrap().line()
        };
        let i = at(&s, "big");
        s.cur_mut().select(i);
        s.toggle_info();
        assert_eq!(line(&s), format!("2,000 items · {}", fs::fmt_size(2000)));
        let i = at(&s, "empty");
        s.cur_mut().select(i);
        s.sync_preview();
        assert_eq!(line(&s), "Empty");
        s.cur_mut().select_all();
        s.sync_preview();
        assert_eq!(line(&s), format!("3 selected · 2,000 items inside · {}", fs::fmt_size(2003)));
        // Nothing selected (its files are hidden): the folder on screen, its contents.
        std::fs::create_dir_all(d.join("quiet")).unwrap();
        std::fs::write(d.join("quiet/.a"), "a").unwrap();
        std::fs::write(d.join("quiet/.b"), "b").unwrap();
        let mut s = state_in(&d.join("quiet"));
        settle_all(&mut s);
        s.toggle_info();
        assert_eq!(line(&s), "2 items · 2 B");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn several_files_say_how_many_are_selected() {
        let t = Total { bytes: 62, selected: 3, done: true, ..Total::default() };
        assert_eq!(t.line(), "3 selected · 62 B");
        let t = Total { files: 1, selected: 2, selected_dirs: 1, ..Total::default() };
        assert_eq!(t.line(), "2 selected · 1 item inside · 0 B so far…");
        let t = Total { unreadable: 1, done: true, ..Total::default() };
        assert_eq!(t.line(), "0 items · 0 B", "a folder it couldn't read isn't called empty");
    }

    #[test]
    fn modes_and_attributes_read_as_ls_and_explorer_say() {
        assert_eq!(mode_string(0o100644), "-rw-r--r-- (644)");
        assert_eq!(mode_string(0o040755), "drwxr-xr-x (755)");
        assert_eq!(mode_string(0o104755), "-rwsr-xr-x (4755)");
        assert_eq!(mode_string(0o041777), "drwxrwxrwt (1777)");
        assert_eq!(mode_string(0o120777), "lrwxrwxrwx (777)");
        assert_eq!(attr_string(0x1 | 0x2), "Read-only, Hidden");
        assert_eq!(attr_string(0x80), "Normal");
        assert_eq!(group(1234567), "1,234,567");
        assert_eq!(group(999), "999");
    }
}
