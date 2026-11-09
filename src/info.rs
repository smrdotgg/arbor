//! Get Info (⌘I): a panel, where the quick preview goes, with what the disk says about the
//! selected item: name, kind, where it is (copyable), size, created, modified and accessed,
//! permissions and owner (Unix) or attributes (Windows), where a link points, and for a folder
//! (or several items) the total size and item count.
//!
//! The total is view-triggered background work and follows NORTHSTAR's rules for it:
//!
//! - **First paint never waits:** the panel shows at once with what the listing knows; the
//!   item's own details land from a worker a moment later, and the total streams in.
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
use crate::delete::{Fence, Mounts};
use crate::fs;

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

/// A walk's running count: what it has found so far, and whether it's finished.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Total {
    pub bytes: u64,
    pub files: u64,
    pub folders: u64,
    /// Folders it couldn't read (no permission, gone meanwhile).
    pub unreadable: u64,
    /// Other file systems mounted inside, not entered.
    pub mounts: u64,
    pub done: bool,
}

impl Total {
    /// "1,234 items · 4.5 GB", with "so far" while it counts.
    pub fn line(&self) -> String {
        let items = self.files + self.folders;
        let mut s = format!("{} item{} · {}", group(items), if items == 1 { "" } else { "s" }, fs::fmt_size(self.bytes));
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

/// Adds up `roots` (folders are walked; anything else counts as itself), never following
/// links or entering another file system, until done or `stop` is set. `seed` is already
/// counted (files whose sizes the listing had). Publishes into `out` as it goes, calling `wake`
/// at most every [`WAKE_EVERY`].
pub fn walk(roots: &[PathBuf], seed: Total, out: &Shared, stop: &AtomicBool, wake: &dyn Fn()) {
    walk_with(roots, seed, out, stop, wake, &Mounts::read());
}

/// [`walk`], with the mount points to stop at (besides another device).
fn walk_with(roots: &[PathBuf], seed: Total, out: &Shared, stop: &AtomicBool, wake: &dyn Fn(), mounts: &Mounts) {
    let mut t = seed;
    let mut last = Instant::now();
    let mut n = 0u32;
    let publish = |t: &Total| {
        if let Ok(mut o) = out.lock() {
            *o = t.clone();
        }
    };
    for root in roots {
        let fence = Fence::new(root, mounts);
        let mut stack = vec![root.clone()];
        t.folders += 1;
        while let Some(dir) = stack.pop() {
            if stop.load(Relaxed) {
                return;
            }
            #[cfg(test)]
            hook::at(&dir);
            let rd = match std::fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(_) => {
                    t.unreadable += 1;
                    continue;
                }
            };
            for de in rd {
                if stop.load(Relaxed) {
                    return;
                }
                let Ok(de) = de else {
                    t.unreadable += 1;
                    break;
                };
                let p = de.path();
                // An lstat: links are counted as themselves, never followed.
                let Ok(m) = p.symlink_metadata() else { continue };
                if m.is_dir() {
                    if fence.crosses(&p, &m) {
                        t.mounts += 1;
                        continue;
                    }
                    t.folders += 1;
                    stack.push(p);
                } else {
                    t.files += 1;
                    t.bytes += m.len();
                }
                n += 1;
                if n.is_multiple_of(64) && last.elapsed() >= WAKE_EVERY {
                    last = Instant::now();
                    publish(&t);
                    wake();
                }
            }
        }
    }
    t.done = true;
    publish(&t);
    wake();
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
        let key = self.info_key();
        let f = self.cur();
        let picked = f.targets();
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
        let walk_roots: Vec<PathBuf> = if picked.is_empty() {
            paths.clone()
        } else {
            picked.iter().filter(|e| e.is_dir && !e.is_link).map(|e| f.cwd.join(&e.os)).collect()
        };
        let seed = Total { bytes: size, files: files as u64, ..Total::default() };
        let stop = Arc::new(AtomicBool::new(false));
        self.next_info += 1;
        let req = self.next_info;
        let meta = if paths.len() == 1 {
            let p = paths[0].clone();
            crate::folder::read_waiting(&self.meta_loader, self.show_wait, move || Some((req, meta(&p)))).map(|(_, m)| m)
        } else {
            self.meta_loader.cancel();
            None
        };
        let total = (!walk_roots.is_empty() || paths.len() > 1).then(|| {
            let shared: Shared = Arc::new(Mutex::new(seed.clone()));
            let (out, stop, waker) = (shared.clone(), stop.clone(), self.waker.clone());
            WALKS.fetch_add(1, Relaxed);
            std::thread::Builder::new()
                .name("arbor-info".into())
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
        self.info = Some(Info { key, dir, paths, name, dirs, files, size, meta, req, total, stop });
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
        let d = std::env::temp_dir().join(format!("arbor-info-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn total_of(roots: &[PathBuf]) -> Total {
        let out: Shared = Arc::default();
        walk(roots, Total::default(), &out, &AtomicBool::new(false), &|| {});
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
        assert_eq!((t.folders, t.files, t.done), (3, 3 + links, true), "{t:?}");
        assert!(t.bytes >= 1525 && t.bytes < 1525 + 4096, "a link counts as itself, never followed: {t:?}");
        assert_eq!(t.line(), format!("{} items · {}", 6 + links, fs::fmt_size(t.bytes)));
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
            assert_eq!((t.files, t.folders), (1, 2));
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
        walk_with(std::slice::from_ref(&d), Total::default(), &out, &AtomicBool::new(false), &|| {}, &Mounts::of(&[&canon.join("disk")]));
        let t = out.lock().unwrap().clone();
        assert_eq!((t.bytes, t.files, t.folders, t.mounts, t.done), (3, 1, 1, 1, true), "{t:?}");
        assert_eq!(t.caveats().as_deref(), Some("1 mounted disk inside not counted"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// On Linux, a real one: walking `/dev` never goes into `/dev/pts` and the like.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_walk_of_dev_stops_at_its_mounts() {
        let out: Shared = Arc::default();
        walk(&[PathBuf::from("/dev")], Total::default(), &out, &AtomicBool::new(false), &|| {});
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
        assert_eq!((t.bytes, t.files, t.folders), (500, 2, 1));
        let _ = std::fs::remove_dir_all(&d);
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
