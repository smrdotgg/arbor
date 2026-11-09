//! Everything that touches the disk or the OS: listing a folder, sorting, opening and revealing.
//! No caching: every navigation reads the folder fresh.

use std::cmp::Ordering;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::UNIX_EPOCH;

#[derive(Clone, Debug)]
pub struct Entry {
    /// The exact on-disk name, for joining paths.
    pub os: OsString,
    /// Display name (lossy UTF-8).
    pub name: String,
    /// Lowercased name, for filtering and case-insensitive sorting.
    pub lower: String,
    /// Virtual-row key: `f:` + name. Never contains `/`, which Rune reads as a key path.
    pub key: String,
    pub is_dir: bool,
    pub is_link: bool,
    pub hidden: bool,
    pub size: u64,
    /// Milliseconds since the Unix epoch; 0 when unknown.
    pub mtime_ms: i64,
    /// What tells this item from another put at its path since it was read ([`ident`]).
    pub id: Option<Ident>,
    /// The name an inline rename is giving it, shown until the worker says how it went: only a
    /// label (`os` is still its real name, which every action uses, and actions on it wait).
    pub pending: Option<String>,
}

/// Object identity: whether what's at a path is still the item chosen from a listing or a
/// search, edited or not. Permanent delete, Empty Trash and every action on a search result
/// compare it right before acting, from the metadata already read for the item (see [`ident`]
/// and [`ident_of`]).
///
/// It's the object, not its contents: device and inode (Windows: creation time and whether
/// it's a folder), type and birth time. Size and times of change aren't part of it, so a file
/// edited after its folder was listed (Arbor doesn't watch folders) is still the item the user
/// chose. Whether an item is *unchanged* is another question, with its own type: undo, redo,
/// batch rename and a cancelled copy's cleanup compare `ops::Print` (identity, type, size,
/// mtime) and refuse on any edit.
///
/// Device and inode alone aren't enough: a file system reuses a freed inode at once (ext4 gave a
/// new file at the same name the old one's inode in 100 of 100 tries). The birth time tells
/// them apart, where the file system keeps one (Linux's statx btime; macOS's birth time;
/// Windows' creation time). Where it keeps none (ext2/3, some network and FUSE file systems, a
/// kernel or sandbox without statx), Linux reads the file system's handle for the item
/// (`name_to_handle_at`: inode and generation on ext4, which a reused inode doesn't share) when
/// a path is at hand ([`ident_of`]), and otherwise compares the inode's ctime too: on those
/// systems alone an item edited since it was chosen reads as another, which refuses rather than
/// guesses. Limits: ext4 stamps from a clock that ticks every few ms, so a replacement made in
/// the tick the original was made in reads as it.
///
/// Comparing (`==`): device and inode, type and birth time must match; then the handles if both
/// sides have one, else the ctimes (both `None` where there's a birth time). So it isn't
/// transitive across identities with and without a handle: it's only ever used to compare one
/// item captured earlier with what's at its path now.
#[derive(Clone, Copy, Debug)]
pub struct Ident {
    /// Unix: device and inode. Windows: creation time (ns) and whether it's a folder.
    node: (u64, u64),
    /// 0 a file, 1 a folder, 2 a link, 3 anything else.
    kind: u8,
    /// When it was made, where the file system says (Linux's statx btime, macOS's birth time,
    /// Windows' creation time), in ns.
    born: Option<i128>,
    /// Unix, only where there's no birth time: when its inode last changed (ctime), in ns.
    changed: Option<i128>,
    /// The file system's own name for the object, where it was read: Linux's file handle
    /// (its type and a hash of its bytes), only where there's no birth time.
    handle: Option<(u64, u64)>,
}

impl PartialEq for Ident {
    fn eq(&self, o: &Ident) -> bool {
        (self.node, self.kind, self.born) == (o.node, o.kind, o.born)
            && match (self.handle, o.handle) {
                (Some(a), Some(b)) => a == b,
                _ => self.changed == o.changed,
            }
    }
}

impl Eq for Ident {}

impl std::hash::Hash for Ident {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        (self.node, self.kind, self.born).hash(h);
    }
}

impl Ident {
    /// From its parts, for metadata read without `std` (`dirfd`). `changed` (ctime) is kept
    /// only without a birth time.
    #[cfg(unix)]
    #[allow(clippy::unnecessary_cast)] // mode_t is u16 on macOS
    pub(crate) fn of_parts(dev: u64, ino: u64, mode: u32, born: Option<i128>, changed: i128) -> Ident {
        let kind = match mode & libc::S_IFMT as u32 {
            m if m == libc::S_IFREG as u32 => 0,
            m if m == libc::S_IFDIR as u32 => 1,
            m if m == libc::S_IFLNK as u32 => 2,
            _ => 3,
        };
        let born = born.filter(|_| !pretend::no_birth_time());
        Ident { node: (dev, ino), kind, born, changed: born.is_none().then_some(changed), handle: None }
    }

    /// Whether there's no birth time in it, so a handle would tell more (Linux).
    pub(crate) fn wants_handle(&self) -> bool {
        cfg!(target_os = "linux") && self.born.is_none()
    }

    /// With the file system's handle for it (`dirfd::handle_key`).
    #[cfg(unix)]
    pub(crate) fn with_handle(self, handle: Option<(u64, u64)>) -> Ident {
        Ident { handle, ..self }
    }
}

/// Tests: makes this thread's identities as a system without birth times (or file handles)
/// would read them, so the fallbacks are tested on any machine. Off outside tests.
pub(crate) mod pretend {
    #[cfg(test)]
    thread_local! {
        static NO_BIRTH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static NO_HANDLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    pub(crate) fn no_birth_time() -> bool {
        #[cfg(test)]
        return NO_BIRTH.with(|c| c.get());
        #[cfg(not(test))]
        false
    }

    #[cfg(unix)]
    pub(crate) fn no_handle() -> bool {
        #[cfg(test)]
        return NO_HANDLE.with(|c| c.get());
        #[cfg(not(test))]
        false
    }

    /// Until it's dropped: no birth times, and with `no_handle` no file handles either.
    #[cfg(all(test, unix))]
    pub(crate) struct Pretend;

    #[cfg(all(test, unix))]
    pub(crate) fn without_birth_times(no_handle: bool) -> Pretend {
        NO_BIRTH.with(|c| c.set(true));
        NO_HANDLE.with(|c| c.set(no_handle));
        Pretend
    }

    #[cfg(all(test, unix))]
    impl Drop for Pretend {
        fn drop(&mut self) {
            NO_BIRTH.with(|c| c.set(false));
            NO_HANDLE.with(|c| c.set(false));
        }
    }
}

/// A time as ns since the Unix epoch (negative before it).
fn ns(t: std::time::SystemTime) -> i128 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}

/// An item's [`Ident`] from its metadata alone (not following a link): no file handle, so
/// where there's no birth time its ctime stands in ([`ident_of`] does better with a path).
/// Windows gives a file created under a name within 15 s of deleting one there the old one's
/// creation time ("tunnelling"), so there a replacement that quick reads as the same item.
pub fn ident(m: &std::fs::Metadata) -> Option<Ident> {
    let ft = m.file_type();
    let kind = if ft.is_symlink() {
        2
    } else if ft.is_dir() {
        1
    } else if ft.is_file() {
        0
    } else {
        3
    };
    // Linux: statx's btime, `Err` where the file system keeps none (or there's no statx).
    let born = m.created().ok().map(ns).filter(|_| !pretend::no_birth_time());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let changed = born.is_none().then(|| m.ctime() as i128 * 1_000_000_000 + m.ctime_nsec() as i128);
        Some(Ident { node: (m.dev(), m.ino()), kind, born, changed, handle: None })
    }
    #[cfg(not(unix))]
    {
        // Without a creation time nothing tells it from any other item: no identity (and
        // nothing is deleted on a guess).
        let b = born?;
        Some(Ident { node: (b as u64, u64::from(m.is_dir())), kind, born, changed: None, handle: None })
    }
}

/// [`ident`] for the item at `path`, whose metadata `m` is: on Linux, where there's no birth
/// time, with the file system's handle for it too (one more call, only there).
pub fn ident_of(path: &Path, m: &std::fs::Metadata) -> Option<Ident> {
    let id = ident(m)?;
    #[cfg(target_os = "linux")]
    if id.wants_handle() {
        let handle = crate::ffi::cstr(path).ok().and_then(|p| crate::dirfd::handle_key(libc::AT_FDCWD, &p, 0));
        return Some(id.with_handle(handle));
    }
    let _ = path;
    Some(id)
}

/// The identity of what's at `path` now (not following a link), `None` when nothing is or it
/// can't be read: compare it with the one captured earlier, right before acting on the path.
pub fn ident_at(path: &Path) -> Option<Ident> {
    ident_of(path, &path.symlink_metadata().ok()?)
}

/// Tests: the file at `p` removed and another made there holding `content`, one that got `p`'s
/// inode back (ext4 hands a new file the lowest inode free), with its own birth time where the
/// file system keeps one. Returns the identity of the item it replaced, which device, inode and
/// type alone can't tell from the new one. `None` when `p` can't be read.
///
/// Tests running alongside free and take inodes too, so it's tried a few times, each newcomer
/// becoming the item to replace next. If the inode never comes back (a busy machine), `p` still
/// holds a newcomer, with another inode, and the identity of the item it replaced is returned:
/// a plain replacement, said on stderr.
#[cfg(all(test, unix))]
pub(crate) fn reuse_inode(p: &Path, content: &str) -> Option<Ident> {
    let mut last = None;
    for _ in 0..5 {
        match reuse_inode_once(p, content)? {
            (id, true) => return Some(id),
            (id, false) => last = Some(id),
        }
    }
    eprintln!("reuse_inode: {} never got its inode back; replaced by a newcomer with another one", p.display());
    last
}

/// One try of [`reuse_inode`]: the identity replaced, and whether the newcomer got its inode.
#[cfg(all(test, unix))]
fn reuse_inode_once(p: &Path, content: &str) -> Option<(Ident, bool)> {
    use std::os::unix::fs::MetadataExt;
    let before = ident_at(p)?;
    let ino = p.symlink_metadata().ok()?.ino();
    // More than a tick of the clock the file system stamps from (ext4: a few ms) after the
    // original was made, so the newcomer's birth time is its own.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::remove_file(p).ok()?;
    // A lower inode freed by a test running alongside comes first: each newcomer that got one
    // is kept aside, filling the gap, until the freed one comes back.
    let mut aside = Vec::new();
    let mut reused = false;
    for i in 0..2000 {
        std::fs::write(p, content).ok()?;
        let m = p.symlink_metadata().ok()?;
        if m.ino() == ino {
            if before.born.is_some() {
                assert_ne!(ident(&m)?.born, before.born, "a newcomer has its own birth time");
            }
            reused = true;
            break;
        }
        let filler = p.with_file_name(format!(".reuse-filler-{i}"));
        // The last newcomer stays at `p`.
        if i < 1999 {
            std::fs::rename(p, &filler).ok()?;
            aside.push(filler);
        }
    }
    for f in aside {
        let _ = std::fs::remove_file(f);
    }
    Some((before, reused))
}

/// Tests: whether the temp folder's file system reuses a freed inode at once, so reuse can be
/// shown; says so when not (APFS and tmpfs don't).
#[cfg(all(test, unix))]
pub(crate) fn inode_reuse_shows(test: &str) -> bool {
    let d = std::env::temp_dir().join(format!("arbor-reuse-probe-{test}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    let p = d.join("probe");
    let shows = std::fs::write(&p, "probe").is_ok() && (0..5).any(|_| reuse_inode_once(&p, "again").is_some_and(|(_, r)| r));
    let _ = std::fs::remove_dir_all(&d);
    if !shows {
        eprintln!("{test}: skipped (the temp folder's file system doesn't reuse an inode at once)");
    }
    shows
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Modified,
    Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub desc: bool,
}

pub struct Listing {
    pub path: PathBuf,
    pub result: Result<Vec<Entry>, String>,
    /// The folder's own identity (links followed), so two panes showing one folder by different
    /// paths (a symlink, a junction) are known to be the same. `None` when it can't be had.
    pub dir_id: Option<crate::noclobber::FileId>,
    /// It couldn't be read because there's no folder there (checked on the worker, only after
    /// a failed read): Jump drops such a folder from its history.
    pub missing: bool,
}

/// Reads `path` and sorts it. Runs on a worker thread.
pub fn list(path: PathBuf, sort: Sort) -> Listing {
    list_until(path, sort, &AtomicBool::new(false)).expect("never stopped")
}

/// [`list`], stopping as soon as it sees `stop` set (checked before each entry and before
/// sorting): `None`, and the rest of the folder isn't read. A tab hidden or closed while its
/// folder is read stops the read this way (NORTHSTAR: leaving a view cancels its work).
pub fn list_until(path: PathBuf, sort: Sort, stop: &AtomicBool) -> Option<Listing> {
    let mut result = read(&path, stop)?;
    if stop.load(Relaxed) {
        return None;
    }
    if let Ok(v) = &mut result {
        sort_entries(v, sort);
    }
    let dir_id = result.is_ok().then(|| crate::noclobber::dir_id(&path)).flatten();
    let missing = result.is_err() && !path.is_dir();
    Some(Listing { path, result, dir_id, missing })
}

/// `None` when stopped. One relaxed load per entry: nothing next to its `lstat`.
fn read(path: &Path, stop: &AtomicBool) -> Option<Result<Vec<Entry>, String>> {
    let rd = match std::fs::read_dir(path) {
        Ok(rd) => rd,
        Err(e) => return Some(Err(e.to_string())),
    };
    let mut out = Vec::with_capacity(64);
    for de in rd.flatten() {
        #[cfg(test)]
        read_hook::entry(path);
        if stop.load(Relaxed) {
            #[cfg(test)]
            read_hook::done(path);
            return None;
        }
        let os = de.file_name();
        let name = os.to_string_lossy().into_owned();
        // `file_type` comes from the directory entry itself on every platform (no stat).
        let ft = de.file_type().ok();
        let is_link = ft.is_some_and(|t| t.is_symlink());
        // One lstat per entry (free on Windows, where read_dir already has it).
        let meta = de.metadata().ok();
        let mut is_dir = ft.is_some_and(|t| t.is_dir());
        let (mut size, mut mtime_ms) = meta.as_ref().map_or((0, 0), |m| (m.len(), mtime(m)));
        if is_link {
            // Follow links once so a link to a folder behaves as a folder.
            if let Ok(target) = std::fs::metadata(de.path()) {
                is_dir = target.is_dir();
                size = target.len();
                mtime_ms = mtime(&target);
            }
        }
        let hidden = is_hidden(&name, meta.as_ref());
        let lower = name.to_lowercase();
        let key = format!("f:{name}");
        // Where there's no birth time (Linux), with the file system's handle: a call more per
        // entry, only on those systems.
        let id = meta
            .as_ref()
            .and_then(ident)
            .map(|id| if id.wants_handle() { ident_of(&de.path(), meta.as_ref().unwrap()).unwrap_or(id) } else { id });
        out.push(Entry { os, name, lower, key, is_dir, is_link, hidden, size: if is_dir { 0 } else { size }, mtime_ms, id, pending: None });
    }
    #[cfg(test)]
    read_hook::done(path);
    Some(Ok(out))
}

/// Test-only: holds a read of one folder at its first entry, and counts the entries it reads.
#[cfg(test)]
pub(crate) mod read_hook {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, Sender};

    pub(crate) struct Gate {
        pub path: PathBuf,
        /// Told when the read reaches its first entry; it then waits for `go`.
        pub started: Sender<()>,
        pub go: Option<Receiver<()>>,
        /// Told how many entries the read looked at, once it ends or stops.
        pub done: Sender<usize>,
        pub seen: usize,
    }

    pub(crate) static GATES: Mutex<Vec<Gate>> = Mutex::new(Vec::new());

    pub(super) fn entry(p: &Path) {
        let go = {
            let mut g = GATES.lock().unwrap();
            let Some(gate) = g.iter_mut().find(|g| g.path == p) else { return };
            gate.seen += 1;
            if gate.seen > 1 {
                return;
            }
            let _ = gate.started.send(());
            gate.go.take()
        };
        if let Some(go) = go {
            let _ = go.recv();
        }
    }

    pub(super) fn done(p: &Path) {
        let mut g = GATES.lock().unwrap();
        if let Some(i) = g.iter().position(|g| g.path == p) {
            let gate = g.remove(i);
            let _ = gate.done.send(gate.seen);
        }
    }
}

pub(crate) fn mtime(m: &std::fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64)
}

#[allow(unused_variables)]
pub(crate) fn is_hidden(name: &str, meta: Option<&std::fs::Metadata>) -> bool {
    if name.starts_with('.') {
        return true;
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        const UF_HIDDEN: u32 = 0x8000;
        if meta.is_some_and(|m| m.st_flags() & UF_HIDDEN != 0) {
            return true;
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        if meta.is_some_and(|m| m.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0) {
            return true;
        }
    }
    false
}

pub fn sort_entries(v: &mut [Entry], sort: Sort) {
    v.sort_unstable_by(|a, b| {
        // Folders first, always.
        let dirs = b.is_dir.cmp(&a.is_dir);
        if dirs != Ordering::Equal {
            return dirs;
        }
        let ord = match sort.key {
            SortKey::Name => Ordering::Equal,
            SortKey::Size => a.size.cmp(&b.size),
            SortKey::Modified => a.mtime_ms.cmp(&b.mtime_ms),
            SortKey::Kind => ext(&a.lower).cmp(ext(&b.lower)),
        }
        .then_with(|| natural(&a.lower, &b.lower))
        .then_with(|| a.name.cmp(&b.name));
        if sort.desc { ord.reverse() } else { ord }
    });
}

pub fn ext(lower: &str) -> &str {
    match lower.rfind('.') {
        Some(0) | None => "",
        Some(i) => &lower[i + 1..],
    }
}

/// Natural order: "file2" before "file10".
fn natural(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let na = a.iter().take_while(|c| c.is_ascii_digit()).count();
                let nb = b.iter().take_while(|c| c.is_ascii_digit()).count();
                let (da, db) = (trim_zeros(&a[..na]), trim_zeros(&b[..nb]));
                let o = da.len().cmp(&db.len()).then_with(|| da.cmp(db));
                if o != Ordering::Equal {
                    return o;
                }
                a = &a[na..];
                b = &b[nb..];
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(y);
                }
                a = &a[1..];
                b = &b[1..];
            }
        }
    }
}

fn trim_zeros(d: &[u8]) -> &[u8] {
    let i = d.iter().position(|&c| c != b'0').unwrap_or(d.len());
    &d[i..]
}

pub fn home() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// The sidebar's default favorites that exist on this machine: home and its usual folders,
/// Applications on macOS, the root elsewhere but Windows. Volumes and drives are the sidebar's
/// Locations (`volumes.rs`).
pub fn places() -> Vec<(String, PathBuf)> {
    let h = home();
    let mut v = vec![("Home".to_string(), h.clone())];
    for d in ["Desktop", "Documents", "Downloads", "Pictures", "Music", "Movies", "Videos"] {
        let p = h.join(d);
        if p.is_dir() {
            v.push((d.to_string(), p));
        }
    }
    #[cfg(target_os = "macos")]
    v.push(("Applications".into(), PathBuf::from("/Applications")));
    #[cfg(not(windows))]
    v.push(("Root".into(), PathBuf::from("/")));
    v
}

/// Expands a leading `~`.
pub fn expand(input: &str) -> PathBuf {
    if input == "~" {
        return home();
    }
    // `~\` only on Windows: elsewhere a backslash is part of a name.
    if let Some(rest) = input.strip_prefix("~/").or_else(|| input.strip_prefix("~\\").filter(|_| cfg!(windows))) {
        return home().join(rest);
    }
    PathBuf::from(input)
}

/// Shows `p` with the home folder as `~`.
pub fn pretty(p: &Path) -> String {
    let h = home();
    match p.strip_prefix(&h) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        Err(_) => p.display().to_string(),
    }
}

/// Opens a file with its default app. Headless runs open nothing, as Rune's `open_url`.
pub fn open(p: &Path) -> Result<(), String> {
    if rune::tooling::headless_run().is_some() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    let cmd = Command::new("open").arg(p).spawn();
    #[cfg(windows)]
    let cmd = Command::new("explorer").arg(p).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = Command::new("xdg-open").arg(p).spawn();
    reap(cmd)
}

/// Opens a text file (the settings file) for editing: in the default text editor on macOS,
/// where a `.toml` file often has no app of its own, else with its default app.
pub fn open_text(p: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    if rune::tooling::headless_run().is_none() {
        return reap(Command::new("open").arg("-t").arg(p).spawn());
    }
    open(p)
}

/// Shows `p` selected in the platform's own file manager.
pub fn reveal(p: &Path) -> Result<(), String> {
    if rune::tooling::headless_run().is_some() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    let cmd = Command::new("open").arg("-R").arg(p).spawn();
    #[cfg(windows)]
    let cmd = {
        // Explorer wants `/select,"C:\a b\c"`: Rust's quoting of the whole argument breaks it, so
        // pass it raw. Windows paths can't contain `"`. `absolute` also turns `/` into `\`.
        use std::os::windows::process::CommandExt;
        let mut arg = OsString::from("/select,\"");
        arg.push(std::path::absolute(p).as_deref().unwrap_or(p));
        arg.push("\"");
        Command::new("explorer").raw_arg(arg).spawn()
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = Command::new("xdg-open").arg(p.parent().unwrap_or(p)).spawn();
    reap(cmd)
}

/// Waits for the child on a throwaway thread so it never lingers as a zombie.
fn reap(cmd: std::io::Result<std::process::Child>) -> Result<(), String> {
    let mut child = cmd.map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub fn fmt_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1000.0;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if v < 10.0 { format!("{v:.1} {}", UNITS[u]) } else { format!("{v:.0} {}", UNITS[u]) }
}

pub fn fmt_time(ms: i64, now: rune::LocalTime) -> String {
    if ms == 0 {
        return "—".into();
    }
    const MON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let t = rune::LocalTime::at(ms);
    let m = MON[(t.month as usize).saturating_sub(1).min(11)];
    if t.year == now.year && t.month == now.month && t.day == now.day {
        format!("Today {:02}:{:02}", t.hour, t.minute)
    } else if t.year == now.year {
        format!("{m} {} {:02}:{:02}", t.day, t.hour, t.minute)
    } else {
        format!("{m} {} {}", t.day, t.year)
    }
}

pub fn kind(e: &Entry) -> &str {
    if e.is_dir {
        return "Folder";
    }
    match ext(&e.lower) {
        "" => "File",
        x => x,
    }
}

/// A file's kind from its name, as the Kind column shows it.
pub fn kind_of(name: &str) -> String {
    match ext(&name.to_lowercase()) {
        "" => "File".into(),
        x => x.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        assert_eq!(natural("file2", "file10"), Ordering::Less);
        assert_eq!(natural("a", "b"), Ordering::Less);
        assert_eq!(natural("x007", "x7"), Ordering::Equal);
        assert_eq!(natural("img12b", "img12a"), Ordering::Greater);
    }

    #[test]
    fn sizes() {
        assert_eq!(fmt_size(999), "999 B");
        assert_eq!(fmt_size(1_500), "1.5 KB");
        assert_eq!(fmt_size(25_000_000), "25 MB");
    }

    /// Reading and sorting a 10k-entry folder, the part of "listing visible" that isn't drawing.
    /// `cargo test --release -- --ignored --nocapture list_10k`
    #[test]
    #[ignore = "benchmark: makes 10,000 files"]
    fn list_10k() {
        let d = std::env::temp_dir().join(format!("arbor-list-10k-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for i in 0..10_000 {
            std::fs::write(d.join(format!("file {i} v{}.txt", i % 7)), b"x").unwrap();
        }
        let sort = Sort { key: SortKey::Name, desc: false };
        let mut times: Vec<f64> = (0..30)
            .map(|_| {
                let t = std::time::Instant::now();
                let l = list(d.clone(), sort);
                assert_eq!(l.result.map(|v| v.len()).unwrap(), 10_000);
                t.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        times.sort_by(f64::total_cmp);
        let mut v = list(d.clone(), sort).result.unwrap();
        let t = std::time::Instant::now();
        sort_entries(&mut v, Sort { key: SortKey::Modified, desc: true });
        let resort = t.elapsed().as_secs_f64() * 1000.0;
        let _ = std::fs::remove_dir_all(&d);
        eprintln!("read+sort 10k: median {:.1} ms, p95 {:.1} ms; re-sort only {resort:.1} ms", times[15], times[28]);
        assert!(times[15] < 50.0, "over the 50 ms listing budget before drawing anything");
    }

    #[test]
    fn lists_a_folder() {
        let l = list(PathBuf::from(env!("CARGO_MANIFEST_DIR")), Sort { key: SortKey::Name, desc: false });
        let v = l.result.unwrap();
        assert!(v.iter().any(|e| e.name == "src" && e.is_dir));
        assert!(v.iter().any(|e| e.name == "Cargo.toml" && !e.is_dir));
        // Folders first.
        let first_file = v.iter().position(|e| !e.is_dir).unwrap();
        assert!(v[first_file..].iter().all(|e| !e.is_dir));
    }
}
