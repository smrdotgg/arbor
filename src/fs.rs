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
    /// Unix: `st_mode` (type and permission bits) from the same `lstat`; Windows: the file
    /// attributes from the directory record. 0 when unknown. (It sits in the padding beside the
    /// flags: an entry is no bigger for it.)
    pub mode: u32,
    /// What tells this item from another put at its path since it was read ([`ident`]).
    pub id: Option<Ident>,
    /// The name an inline rename is giving it, shown until the worker says how it went: only a
    /// label (`os` is still its real name, which every action uses, and actions on it wait).
    pub pending: Option<String>,
}

impl Entry {
    /// When it was made, ms since the Unix epoch (the Date Created column): the birth time its
    /// identity holds (Linux's statx btime, macOS's birth time, Windows' creation time), the
    /// item's own, not a link's target's; 0 where the file system keeps none. Read from its
    /// identity, so a listing keeps nothing more for it.
    pub fn born_ms(&self) -> i64 {
        self.id.and_then(|i| i.born).map_or(0, |ns| (ns / 1_000_000).max(0) as i64)
    }
}

#[cfg(test)]
impl Entry {
    /// An entry as a listing would have it, without a disk, for tests.
    pub fn test(name: &str, is_dir: bool) -> Entry {
        Entry {
            os: name.into(),
            name: name.into(),
            lower: name.to_lowercase(),
            key: format!("f:{name}"),
            is_dir,
            is_link: false,
            hidden: name.starts_with('.'),
            size: 0,
            mtime_ms: 0,
            mode: 0,
            id: None,
            pending: None,
        }
    }
}

/// Object identity: whether what's at a path is still the item chosen from a listing or a
/// search, edited or not. It's read for each entry when a folder is listed (or the search walk
/// finds it), from the metadata read anyway, and compared on the worker right before each item
/// is acted on: trash, move (paste of a cut, F6, a drop), copy (paste, F5, Duplicate), inline
/// rename, permanent delete, Put Back (`ops::Op::Checked`, `ops::Doomed`) and what a drag
/// carries; Empty Trash compares the one its count read. Batch rename compares the fuller
/// `ops::Seen` from its preview. See [`ident`] and [`ident_of`].
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
    /// The file system's own name for the object, where it was read: Linux's file handle (its
    /// type and a hash of its bytes), only where there's no birth time; Windows' volume serial
    /// number and file id, from a listing or a check right before acting.
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

    /// Windows, from a directory record ([`win_dir`]): as [`ident`] gives it from the same
    /// item's metadata, plus its volume and file id.
    #[cfg(windows)]
    pub(crate) fn of_windows(kind: u8, born: i128, is_dir: bool, handle: Option<(u64, u64)>) -> Ident {
        Ident { node: (born as u64, u64::from(is_dir)), kind, born: Some(born), changed: None, handle }
    }

    /// Whether it tells an item from a replacement by its ctime alone (no birth time, no
    /// handle), so Arbor's own rename, move or trash of it (or of another name for it) changes it.
    pub(crate) fn by_ctime(&self) -> bool {
        self.changed.is_some() && self.handle.is_none()
    }

    /// The same device and inode, type and birth time: the same object, whatever its ctime.
    pub(crate) fn same_but_ctime(&self, o: &Ident) -> bool {
        (self.node, self.kind, self.born) == (o.node, o.kind, o.born)
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

/// [`ident`] for the item at `path`, whose metadata `m` is, with the file system's own name for
/// it where that tells more: on Linux, where there's no birth time, its handle (one more call,
/// only there); on Windows its volume and file id (one open, without reading or locking it), to
/// compare with what a listing read ([`win_dir`]). For checks right before acting, not walks.
pub fn ident_of(path: &Path, m: &std::fs::Metadata) -> Option<Ident> {
    #[cfg(test)]
    reads::count();
    let id = ident(m)?;
    #[cfg(target_os = "linux")]
    if id.wants_handle() {
        let handle = crate::ffi::cstr(path).ok().and_then(|p| crate::dirfd::handle_key(libc::AT_FDCWD, &p, 0));
        return Some(id.with_handle(handle));
    }
    #[cfg(windows)]
    {
        let handle = crate::noclobber::file_id(path, m).and_then(|(v, f)| win_dir::file_key(v, f));
        return Some(Ident { handle, ..id });
    }
    #[allow(unreachable_code)]
    {
        let _ = path;
        Some(id)
    }
}

/// Windows: the [`Ident`] of the item open as `f` (a link's own handle, opened with
/// `FILE_FLAG_OPEN_REPARSE_POINT`, is the link): as [`ident_of`] reads it by path, but through
/// the handle, so it's the object held open and nothing else.
#[cfg(windows)]
pub fn ident_of_file(f: &std::fs::File) -> Option<Ident> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
    let id = ident(&f.metadata().ok()?)?;
    // SAFETY: a zeroed record is a valid out-buffer for an open handle.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
        return None;
    }
    let file = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Some(Ident { handle: win_dir::file_key(u64::from(info.dwVolumeSerialNumber), file), ..id })
}

/// Tests: how many identities this thread has read from the disk ([`ident_of`], [`ident_at`]).
#[cfg(test)]
pub(crate) mod reads {
    thread_local! {
        static N: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    pub(crate) fn count() {
        N.with(|n| n.set(n.get() + 1));
    }
    pub(crate) fn so_far() -> u64 {
        N.with(|n| n.get())
    }
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
    /// When it was made (the birth time); items without one sort as the oldest.
    Created,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub desc: bool,
    /// Folders above files whatever the key and direction ("Folders First", `folders_first`).
    pub dirs_first: bool,
}

impl Sort {
    /// By `key`, folders first (the default).
    pub const fn new(key: SortKey, desc: bool) -> Sort {
        Sort { key, desc, dirs_first: true }
    }
}

pub struct Listing {
    pub path: PathBuf,
    pub result: Result<Vec<Entry>, String>,
    /// The folder's own identity (links followed), so two panes showing one folder by different
    /// paths (a symlink, a junction) are known to be the same. `None` when it can't be had.
    pub dir_id: Option<crate::noclobber::DirId>,
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
#[cfg(not(windows))]
fn read(path: &Path, stop: &AtomicBool) -> Option<Result<Vec<Entry>, String>> {
    read_std(path, stop)
}

/// Windows: the folder's entries with their file ids, at no cost per entry ([`win_dir`]); where
/// the file system can't give them that way (some network and FUSE ones), as everywhere else.
#[cfg(windows)]
fn read(path: &Path, stop: &AtomicBool) -> Option<Result<Vec<Entry>, String>> {
    match win_dir::read(path, stop) {
        Ok(Some(v)) => Some(Ok(v)),
        Ok(None) => None,
        Err(()) => read_std(path, stop),
    }
}

/// [`read`] through `std::fs::read_dir`: one `lstat` (Linux: `statx`) per entry, free on
/// Windows, where the directory listing already has it.
fn read_std(path: &Path, stop: &AtomicBool) -> Option<Result<Vec<Entry>, String>> {
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
        // The Permissions column's mode, from the same metadata (no call; Date Created is the
        // identity's birth time).
        let mode = meta.as_ref().map_or(0, mode_of);
        let size = if is_dir { 0 } else { size };
        out.push(Entry { os, name, lower, key, is_dir, is_link, hidden, size, mtime_ms, mode, id, pending: None });
    }
    #[cfg(test)]
    read_hook::done(path);
    Some(Ok(out))
}

/// Windows' listing with file ids: `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` on
/// the folder fills a buffer with entries (name, times, size, attributes, reparse tag and the
/// file system's file id), as `FindFirstFileEx(FindExInfoBasic)` does without the id (and, like
/// it, without the 8.3 short names `FileIdBothDirectoryInfo` would make NTFS look up); the
/// volume's serial number comes once per folder. A file system that can't answer it (FAT, some
/// network and FUSE ones) is read `std`'s way, without file ids. So each entry's identity has its volume and file id, which a file made
/// under the name within NTFS's 15 s creation-time tunnelling doesn't share.
#[cfg(windows)]
pub(crate) mod win_dir {
    use super::*;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_NO_MORE_FILES, GetLastError};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_EXTD_DIR_INFO, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FileIdExtdDirectoryInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    };

    /// From FILETIME's epoch (1601) to Unix's, in 100 ns.
    const EPOCH: i64 = 116_444_736_000_000_000;
    /// A reparse tag with this bit names another item (a symlink, a junction): `std`'s link.
    const NAME_SURROGATE: u32 = 0x2000_0000;

    /// `Err` when the folder can't be read this way (then `std`'s way says why); `Ok(None)`
    /// when stopped.
    pub(crate) fn read(path: &Path, stop: &AtomicBool) -> Result<Option<Vec<Entry>>, ()> {
        let dir = std::fs::OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .map_err(|_| ())?;
        let h = dir.as_raw_handle();
        // SAFETY: a zeroed BY_HANDLE_FILE_INFORMATION is a valid out-buffer for an open handle.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(h, &mut info) } == 0 || info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(());
        }
        let volume = u64::from(info.dwVolumeSerialNumber);
        // u64s, so each record's 8-byte fields are aligned as the call writes them.
        let mut buf = vec![0u64; 64 * 1024 / 8];
        let len = buf.len() * 8;
        let fixed = std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
        let mut out = Vec::with_capacity(64);
        loop {
            // SAFETY: `buf` is writable for `len` bytes.
            if unsafe { GetFileInformationByHandleEx(h, FileIdExtdDirectoryInfo, buf.as_mut_ptr().cast(), len as u32) } == 0 {
                if unsafe { GetLastError() } == ERROR_NO_MORE_FILES {
                    break;
                }
                return Err(());
            }
            // SAFETY: the call filled `buf`; it's read as bytes, each record bounds-checked.
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), len) };
            let mut at = 0;
            loop {
                if at + fixed > len {
                    return Err(());
                }
                // SAFETY: `at + fixed` bytes are in `bytes`; the record is copied out unaligned.
                let r: FILE_ID_EXTD_DIR_INFO = unsafe { std::ptr::read_unaligned(bytes.as_ptr().add(at).cast()) };
                let n = r.FileNameLength as usize;
                if at + fixed + n > len {
                    return Err(());
                }
                let wide: Vec<u16> = bytes[at + fixed..at + fixed + n].as_chunks::<2>().0.iter().map(|&c| u16::from_ne_bytes(c)).collect();
                if wide != [0x2e] && wide != [0x2e, 0x2e] {
                    #[cfg(test)]
                    read_hook::entry(path);
                    if stop.load(Relaxed) {
                        #[cfg(test)]
                        read_hook::done(path);
                        return Ok(None);
                    }
                    out.push(entry(path, OsString::from_wide(&wide), &r, volume));
                }
                if r.NextEntryOffset == 0 {
                    break;
                }
                at += r.NextEntryOffset as usize;
            }
        }
        #[cfg(test)]
        read_hook::done(path);
        Ok(Some(out))
    }

    /// One record as an [`Entry`], as `read_std` would make it (plus the file id).
    fn entry(dir: &Path, os: OsString, r: &FILE_ID_EXTD_DIR_INFO, volume: u64) -> Entry {
        let attrs = r.FileAttributes;
        let is_link = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 && r.ReparsePointTag & NAME_SURROGATE != 0;
        let own_dir = !is_link && attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
        let (mut is_dir, mut size, mut mtime_ms) = (own_dir, r.EndOfFile as u64, ms(r.LastWriteTime));
        if is_link {
            // Follow links once so a link to a folder behaves as a folder.
            if let Ok(t) = std::fs::metadata(dir.join(&os)) {
                is_dir = t.is_dir();
                size = t.len();
                mtime_ms = mtime(&t);
            }
        }
        let name = os.to_string_lossy().into_owned();
        let hidden = name.starts_with('.') || attrs & FILE_ATTRIBUTE_HIDDEN != 0;
        let born = (i128::from(r.CreationTime) - i128::from(EPOCH)) * 100;
        let kind = if is_link { 2 } else { u8::from(own_dir) };
        // A 128-bit id: NTFS's 64-bit file reference in its low half (what a handle's file index
        // is); one that needs the high half too (ReFS) isn't used.
        let (low, high) = r.FileId.Identifier.split_at(8);
        let file = high.iter().all(|&b| b == 0).then(|| u64::from_le_bytes(low.try_into().unwrap()));
        let id = Ident::of_windows(kind, born, own_dir, file.and_then(|f| file_key(volume, f)));
        let lower = name.to_lowercase();
        let key = format!("f:{name}");
        let size = if is_dir { 0 } else { size };
        Entry { os, name, lower, key, is_dir, is_link, hidden, size, mtime_ms, mode: attrs, id: Some(id), pending: None }
    }

    /// A FILETIME as ms since the Unix epoch, 0 before it (as [`mtime`] gives it).
    fn ms(ft: i64) -> i64 {
        if ft <= EPOCH { 0 } else { (ft - EPOCH) / 10_000 }
    }

    /// Volume and file id, unless the file system gives none (0, or all ones where ReFS's
    /// 128-bit id doesn't fit).
    pub(crate) fn file_key(volume: u64, id: u64) -> Option<(u64, u64)> {
        (id != 0 && id != u64::MAX).then_some((volume, id))
    }
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

/// [`Entry::mode`] from an item's own metadata: Unix's `st_mode`, Windows' attributes.
pub(crate) fn mode_of(m: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        m.mode()
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        m.file_attributes()
    }
}

/// The Permissions column: `drwxr-xr-x` on Unix (as `ls -l`), the attributes' letters on
/// Windows (`RHSA`, as Explorer's Attributes column), "—" when unknown.
pub fn fmt_mode(e: &Entry) -> String {
    if e.mode == 0 {
        return "—".into();
    }
    if cfg!(windows) { fmt_attrs(e.mode) } else { fmt_unix_mode(e.mode) }
}

/// `st_mode` as `ls -l` writes it: the type, then owner, group and others' rwx, with setuid,
/// setgid and sticky as s/S and t/T.
pub fn fmt_unix_mode(m: u32) -> String {
    let kind = match m & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        0o010000 => 'p',
        0o140000 => 's',
        0o020000 => 'c',
        0o060000 => 'b',
        _ => '-',
    };
    let mut s = String::with_capacity(10);
    s.push(kind);
    for (shift, special, mark) in [(6, 0o4000, 's'), (3, 0o2000, 's'), (0, 0o1000, 't')] {
        let bits = (m >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(match (bits & 1 != 0, m & special != 0) {
            (true, true) => mark,
            (false, true) => mark.to_ascii_uppercase(),
            (true, false) => 'x',
            (false, false) => '-',
        });
    }
    s
}

/// Windows' file attributes as Explorer's Attributes column writes them: R (read-only), H
/// (hidden), S (system), A (archive), those set, in that order; "—" when none is.
pub fn fmt_attrs(a: u32) -> String {
    let s: String = [(0x1, 'R'), (0x2, 'H'), (0x4, 'S'), (0x20, 'A')].iter().filter(|(b, _)| a & b != 0).map(|(_, c)| *c).collect();
    if s.is_empty() { "—".into() } else { s }
}

/// The Extension column: what follows the name's last dot, as written ("JPG", "tar.gz" is
/// "gz"); "—" for a folder or a name without one (a leading dot isn't one: `.bashrc`).
pub fn shown_ext(e: &Entry) -> &str {
    if e.is_dir {
        return "—";
    }
    match e.name.rfind('.') {
        Some(0) | None => "—",
        Some(i) if i + 1 == e.name.len() => "—",
        Some(i) => &e.name[i + 1..],
    }
}

/// The space free to this user on the volume holding `path`, in bytes: one `statvfs`
/// (`GetDiskFreeSpaceExW` on Windows). `None` when it can't be read. Run on a worker: a dead
/// network volume can hold it up.
pub fn free_space(path: &Path) -> Option<u64> {
    #[cfg(test)]
    free_reads::count(path);
    #[cfg(unix)]
    {
        let c = crate::ffi::cstr(path).ok()?;
        // SAFETY: a zeroed statvfs is a valid out-buffer; `c` is a NUL-terminated path.
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)] // the fields' types differ by platform
        Some(st.f_bavail as u64 * st.f_frsize as u64)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let w = crate::ffi::wide(path);
        let mut avail = 0u64;
        // SAFETY: `w` is NUL-terminated; the out-pointers are valid or null (not wanted).
        let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, std::ptr::null_mut(), std::ptr::null_mut()) };
        (ok != 0).then_some(avail)
    }
}

/// Tests: which threads read each folder's free space ([`free_space`]), in order.
#[cfg(test)]
pub(crate) mod free_reads {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::thread::ThreadId;

    static N: Mutex<Option<HashMap<PathBuf, Vec<ThreadId>>>> = Mutex::new(None);

    pub(super) fn count(p: &Path) {
        let mut n = N.lock().unwrap_or_else(|e| e.into_inner());
        n.get_or_insert_with(HashMap::new).entry(p.to_path_buf()).or_default().push(std::thread::current().id());
    }

    pub(crate) fn of(p: &Path) -> Vec<ThreadId> {
        N.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|n| n.get(p).cloned()).unwrap_or_default()
    }
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
        // Folders first (whichever way the rest goes), unless that's turned off.
        if sort.dirs_first {
            let dirs = b.is_dir.cmp(&a.is_dir);
            if dirs != Ordering::Equal {
                return dirs;
            }
        }
        let ord = match sort.key {
            SortKey::Name => Ordering::Equal,
            SortKey::Size => a.size.cmp(&b.size),
            SortKey::Modified => a.mtime_ms.cmp(&b.mtime_ms),
            // Mixed with files, a folder's kind is "folder" (as the Kind column says).
            SortKey::Kind if !sort.dirs_first => sort_kind(a).cmp(sort_kind(b)),
            SortKey::Kind => ext(&a.lower).cmp(ext(&b.lower)),
            SortKey::Created => a.born_ms().cmp(&b.born_ms()),
        }
        .then_with(|| natural(&a.lower, &b.lower))
        .then_with(|| a.name.cmp(&b.name));
        if sort.desc { ord.reverse() } else { ord }
    });
}

/// What a Kind sort compares when folders mix with files: a folder's kind is "folder", among
/// the files' extensions (as Finder sorts kinds without folders on top).
fn sort_kind(e: &Entry) -> &str {
    if e.is_dir { "folder" } else { ext(&e.lower) }
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
    let cmd = Command::new("open").arg(p).spawn().map_err(|e| e.to_string());
    #[cfg(windows)]
    let cmd = Command::new("explorer").arg(p).spawn().map_err(|e| e.to_string());
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = spawn_opener(p, std::env::var_os("PATH").as_deref());
    reap(cmd)
}

/// Opens a system URL Rune's `open_url` won't (it opens web and mail links only): macOS's
/// `x-apple.systempreferences:` panes, with `open`. Tests and headless runs open nothing (tests
/// keep what they'd have opened: [`opened_urls`]).
#[cfg(not(test))]
pub fn open_url(url: &str) -> Result<(), String> {
    if rune::tooling::headless_run().is_some() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    return reap(Command::new("open").arg(url).spawn().map_err(|e| e.to_string()));
    #[cfg(not(target_os = "macos"))]
    Err(format!("{url} opens only on macOS"))
}

#[cfg(test)]
pub fn open_url(url: &str) -> Result<(), String> {
    OPENED.lock().unwrap_or_else(|e| e.into_inner()).push(url.to_string());
    Ok(())
}

#[cfg(test)]
static OPENED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// The URLs [`open_url`] was asked to open in this test process, taken.
#[cfg(test)]
pub(crate) fn opened_urls() -> Vec<String> {
    std::mem::take(&mut *OPENED.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Opens a text file (the settings file) for editing: in the default text editor on macOS,
/// where a `.toml` file often has no app of its own, else with its default app.
pub fn open_text(p: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    if rune::tooling::headless_run().is_none() {
        return reap(Command::new("open").arg("-t").arg(p).spawn().map_err(|e| e.to_string()));
    }
    open(p)
}

/// Shows `p` selected in the platform's own file manager.
pub fn reveal(p: &Path) -> Result<(), String> {
    if rune::tooling::headless_run().is_some() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    let cmd = Command::new("open").arg("-R").arg(p).spawn().map_err(|e| e.to_string());
    #[cfg(windows)]
    let cmd = {
        // Explorer wants `/select,"C:\a b\c"`: Rust's quoting of the whole argument breaks it, so
        // pass it raw. Windows paths can't contain `"`. `absolute` also turns `/` into `\`.
        use std::os::windows::process::CommandExt;
        let mut arg = OsString::from("/select,\"");
        arg.push(std::path::absolute(p).as_deref().unwrap_or(p));
        arg.push("\"");
        Command::new("explorer").raw_arg(arg).spawn().map_err(|e| e.to_string())
    };
    // Linux has no common way to select an item in "the" file manager: its folder opens.
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = spawn_opener(p.parent().unwrap_or(p), std::env::var_os("PATH").as_deref());
    reap(cmd)
}

/// Linux's openers, in the order tried, each with what goes before the path: `xdg-open`, then
/// GLib's `gio open` (desktops and minimal installs without xdg-utils often have it).
#[cfg(all(unix, not(target_os = "macos")))]
const OPENERS: [(&str, &[&str]); 2] = [("xdg-open", &[]), ("gio", &["open", "--"])];

/// What opening says when there's no opener at all.
#[cfg(all(unix, not(target_os = "macos")))]
pub const NO_OPENER: &str = "neither xdg-open nor gio is installed (xdg-utils or GLib provides one)";

/// The openers on `path_var` (PATH's value), in [`OPENERS`]' order: an executable file in an
/// absolute folder of it (a relative one, `.` say, would run whatever is where Arbor started).
#[cfg(all(unix, not(target_os = "macos")))]
fn openers(path_var: Option<&std::ffi::OsStr>) -> Vec<(PathBuf, &'static [&'static str])> {
    use std::os::unix::fs::PermissionsExt;
    let Some(path_var) = path_var else { return Vec::new() };
    let runnable = |p: &Path| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    OPENERS
        .iter()
        .filter_map(|&(prog, args)| {
            let found = std::env::split_paths(path_var).filter(|d| d.is_absolute()).map(|d| d.join(prog)).find(|p| runnable(p))?;
            Some((found, args))
        })
        .collect()
}

/// Starts the first of [`openers`] that starts, on `p`. The path is an argument of its own,
/// never through a shell. With none, [`NO_OPENER`]; with some that wouldn't start, why each.
#[cfg(all(unix, not(target_os = "macos")))]
fn spawn_opener(p: &Path, path_var: Option<&std::ffi::OsStr>) -> Result<std::process::Child, String> {
    let mut failed = Vec::new();
    for (prog, args) in openers(path_var) {
        match Command::new(&prog).args(args).arg(p).spawn() {
            Ok(child) => return Ok(child),
            Err(e) => failed.push(format!("{}: {e}", prog.display())),
        }
    }
    Err(if failed.is_empty() { NO_OPENER.into() } else { failed.join("; ") })
}

/// Waits for the child on a throwaway thread so it never lingers as a zombie.
fn reap(cmd: Result<std::process::Child, String>) -> Result<(), String> {
    let mut child = cmd?;
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

    /// The QA run's case: on a Linux without `xdg-open` (but with `gio`), opening a file and the
    /// settings did nothing ("No such file or directory"). The openers are tried in order on a
    /// fake PATH: `xdg-open`, then `gio open`, and with neither a message naming both. A
    /// relative PATH folder and a file that can't run are passed over.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_opens_with_xdg_open_then_gio() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("arbor-openers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let (a, b, none) = (d.join("a"), d.join("b"), d.join("none"));
        for x in [&a, &b, &none] {
            std::fs::create_dir_all(x).unwrap();
        }
        let tool = |at: &Path, name: &str, mode: u32| {
            std::fs::write(at.join(name), "#!/bin/sh\nprintf '%s\\n' \"$0\" \"$@\" > \"$(dirname \"$0\")/argv\"\n").unwrap();
            std::fs::set_permissions(at.join(name), std::fs::Permissions::from_mode(mode)).unwrap();
        };
        let path = |dirs: &[&Path]| std::env::join_paths(dirs).unwrap();
        let names = |dirs: &[&Path]| -> Vec<(PathBuf, Vec<&str>)> {
            openers(Some(&path(dirs))).into_iter().map(|(p, args)| (p, args.to_vec())).collect()
        };
        tool(&b, "gio", 0o755);
        assert_eq!(names(&[&none, &b]), [(b.join("gio"), vec!["open", "--"])], "gio when there's no xdg-open");
        tool(&a, "xdg-open", 0o644);
        assert_eq!(names(&[&a, &b]), [(b.join("gio"), vec!["open", "--"])], "an xdg-open that can't run is passed over");
        tool(&a, "xdg-open", 0o755);
        assert_eq!(names(&[&a, &b]), [(a.join("xdg-open"), vec![]), (b.join("gio"), vec!["open", "--"])], "xdg-open first");
        assert!(names(&[Path::new("a"), &none]).is_empty(), "a relative PATH folder is never searched");
        assert_eq!(spawn_opener(Path::new("/x"), Some(&path(&[&none]))).err().as_deref(), Some(NO_OPENER));
        assert!(NO_OPENER.contains("xdg-open") && NO_OPENER.contains("gio"));
        assert_eq!(spawn_opener(Path::new("/x"), None).err().as_deref(), Some(NO_OPENER));
        // The path reaches gio as one argument of its own, untouched by any shell.
        let odd = d.join("a b; $(touch pwned) 'q'.txt");
        let ran = |dirs: &[&Path], at: &Path| {
            let _ = std::fs::remove_file(at.join("argv"));
            // A file just written can be "busy" for a moment while another test's thread forks.
            let mut child = (0..50)
                .find_map(|_| match spawn_opener(&odd, Some(&path(dirs))) {
                    Err(e) if e.contains("busy") => {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        None
                    }
                    other => Some(other),
                })
                .unwrap()
                .unwrap();
            assert!(child.wait().unwrap().success());
            std::fs::read_to_string(at.join("argv")).unwrap()
        };
        let argv = ran(&[&none, &b], &b);
        assert_eq!(argv, format!("{}\nopen\n--\n{}\n", b.join("gio").display(), odd.display()));
        assert!(!d.join("pwned").exists() && !b.join("pwned").exists());
        let argv = ran(&[&a, &b], &a);
        assert_eq!(argv, format!("{}\n{}\n", a.join("xdg-open").display(), odd.display()));
        let _ = std::fs::remove_dir_all(&d);
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
        let sort = Sort::new(SortKey::Name, false);
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
        sort_entries(&mut v, Sort::new(SortKey::Modified, true));
        let resort = t.elapsed().as_secs_f64() * 1000.0;
        eprintln!("read+sort 10k: median {:.1} ms, p95 {:.1} ms; re-sort only {resort:.1} ms", times[15], times[28]);
        // Linux without birth times (a file handle per entry): what those systems pay.
        #[cfg(target_os = "linux")]
        {
            let _p = pretend::without_birth_times(false);
            let mut t: Vec<f64> = (0..30)
                .map(|_| {
                    let t = std::time::Instant::now();
                    let l = list(d.clone(), sort);
                    assert!(l.result.unwrap()[0].id.unwrap().handle.is_some());
                    t.elapsed().as_secs_f64() * 1000.0
                })
                .collect();
            t.sort_by(f64::total_cmp);
            eprintln!("  without birth times (a file handle each): median {:.1} ms, p95 {:.1} ms", t[15], t[28]);
        }
        // Windows: against `std`'s read (FindFirstFileEx, no file ids), on the same folder, taking
        // turns so neither has the warmer cache.
        #[cfg(windows)]
        {
            let (mut ours, mut theirs) = (Vec::new(), Vec::new());
            for _ in 0..30 {
                let t = std::time::Instant::now();
                let mut v = read_std(&d, &AtomicBool::new(false)).unwrap().unwrap();
                sort_entries(&mut v, sort);
                theirs.push(t.elapsed().as_secs_f64() * 1000.0);
                let t = std::time::Instant::now();
                let l = list(d.clone(), sort);
                assert!(l.result.unwrap()[0].id.unwrap().handle.is_some());
                ours.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            ours.sort_by(f64::total_cmp);
            theirs.sort_by(f64::total_cmp);
            eprintln!(
                "  taking turns: with file ids median {:.1} ms (p95 {:.1}); std's read median {:.1} ms (p95 {:.1})",
                ours[15], ours[28], theirs[15], theirs[28]
            );
        }
        let _ = std::fs::remove_dir_all(&d);
        assert!(times[15] < 50.0, "over the 50 ms listing budget before drawing anything");
    }

    /// Windows reads folders with file ids ([`win_dir`]): each entry is what `std`'s read gives,
    /// and its identity, file id and all, is what a check right before acting reads.
    #[cfg(windows)]
    #[test]
    fn windows_listing_with_file_ids_agrees_with_std() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = std::env::temp_dir().join(format!("arbor-win-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("folder")).unwrap();
        std::fs::write(d.join("file.txt"), "12345").unwrap();
        std::fs::write(d.join(".dot"), "").unwrap();
        std::fs::OpenOptions::new().write(true).create_new(true).attributes(0x2).open(d.join("hidden")).unwrap();
        for i in 0..3000 {
            std::fs::write(d.join(format!("many {i}")), "x").unwrap();
        }
        let stop = AtomicBool::new(false);
        let mut ours = win_dir::read(&d, &stop).expect("NTFS gives file ids").unwrap();
        let mut theirs = read_std(&d, &stop).unwrap().unwrap();
        ours.sort_by(|a, b| a.os.cmp(&b.os));
        theirs.sort_by(|a, b| a.os.cmp(&b.os));
        assert_eq!(ours.len(), 3004);
        assert_eq!(ours.len(), theirs.len());
        for (a, b) in ours.iter().zip(&theirs) {
            let what = (&a.os, a.is_dir, a.is_link, a.hidden, a.size, a.mtime_ms, a.born_ms(), a.mode);
            assert_eq!(what, (&b.os, b.is_dir, b.is_link, b.hidden, b.size, b.mtime_ms, b.born_ms(), b.mode));
            assert_eq!(a.id, b.id, "{:?}", a.os);
            let id = a.id.unwrap();
            assert!(id.handle.is_some(), "{:?}: a file id", a.os);
            assert_eq!(Some(id), ident_at(&d.join(&a.os)), "{:?}: as a check reads it", a.os);
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn lists_a_folder() {
        let l = list(PathBuf::from(env!("CARGO_MANIFEST_DIR")), Sort::new(SortKey::Name, false));
        let v = l.result.unwrap();
        assert!(v.iter().any(|e| e.name == "src" && e.is_dir));
        assert!(v.iter().any(|e| e.name == "Cargo.toml" && !e.is_dir));
        // Folders first.
        let first_file = v.iter().position(|e| !e.is_dir).unwrap();
        assert!(v[first_file..].iter().all(|e| !e.is_dir));
    }

    /// Folders First keeps every folder above every file under each sort, either way round;
    /// off, they mix, each key ordering folders and files alike (a folder's kind is "folder").
    #[test]
    fn folders_first_under_every_sort() {
        let born =
            |t: i64| Some(Ident { node: (1, t as u64), kind: 0, born: Some(i128::from(t) * 2_000_000), changed: None, handle: None });
        let mk = |name: &str, dir: bool, size: u64, t: i64| Entry { size, mtime_ms: t, id: born(t), ..Entry::test(name, dir) };
        let all = vec![
            mk("b dir", true, 0, 30),
            mk("a.txt", false, 500, 10),
            mk("z.zip", false, 100, 40),
            mk("c dir", true, 0, 5),
            mk("m.flac", false, 900, 20),
            mk("noext", false, 1, 50),
        ];
        for key in [SortKey::Name, SortKey::Kind, SortKey::Size, SortKey::Modified, SortKey::Created] {
            for desc in [false, true] {
                let mut v = all.clone();
                sort_entries(&mut v, Sort::new(key, desc));
                let first_file = v.iter().position(|e| !e.is_dir).unwrap();
                assert_eq!(first_file, 2, "{key:?} desc={desc}: {:?}", v.iter().map(|e| &e.name).collect::<Vec<_>>());
                assert!(v[first_file..].iter().all(|e| !e.is_dir));
                let mut mixed = all.clone();
                sort_entries(&mut mixed, Sort { dirs_first: false, ..Sort::new(key, desc) });
                let names: Vec<&str> = mixed.iter().map(|e| e.name.as_str()).collect();
                let want: &[&str] = match (key, desc) {
                    (SortKey::Name, false) => &["a.txt", "b dir", "c dir", "m.flac", "noext", "z.zip"],
                    (SortKey::Name, true) => &["z.zip", "noext", "m.flac", "c dir", "b dir", "a.txt"],
                    // No extension first, then flac, folder, txt, zip.
                    (SortKey::Kind, false) => &["noext", "m.flac", "b dir", "c dir", "a.txt", "z.zip"],
                    // Reversed whole, ties (by name) too, as before.
                    (SortKey::Size, true) => &["m.flac", "a.txt", "z.zip", "noext", "c dir", "b dir"],
                    (SortKey::Modified, true) | (SortKey::Created, true) => &["noext", "z.zip", "b dir", "m.flac", "a.txt", "c dir"],
                    (SortKey::Modified, false) | (SortKey::Created, false) => &["c dir", "a.txt", "m.flac", "b dir", "z.zip", "noext"],
                    _ => continue,
                };
                assert_eq!(names, want, "{key:?} desc={desc}");
            }
        }
    }

    #[test]
    fn permissions_and_extensions_read_as_ls_and_explorer_write_them() {
        assert_eq!(fmt_unix_mode(0o100644), "-rw-r--r--");
        assert_eq!(fmt_unix_mode(0o040755), "drwxr-xr-x");
        assert_eq!(fmt_unix_mode(0o120777), "lrwxrwxrwx");
        assert_eq!(fmt_unix_mode(0o104755), "-rwsr-xr-x");
        assert_eq!(fmt_unix_mode(0o102644), "-rw-r-Sr--");
        assert_eq!(fmt_unix_mode(0o041777), "drwxrwxrwt");
        assert_eq!(fmt_unix_mode(0o041776), "drwxrwxrwT");
        assert_eq!(fmt_unix_mode(0o010600), "prw-------");
        assert_eq!(fmt_attrs(0x20), "A");
        assert_eq!(fmt_attrs(0x1 | 0x2 | 0x4 | 0x20 | 0x10), "RHSA");
        assert_eq!(fmt_attrs(0x80), "—", "normal: none of them");
        assert_eq!(fmt_mode(&Entry::test("x", false)), "—", "unknown");
        let ext = |name: &str, dir: bool| shown_ext(&Entry::test(name, dir)).to_string();
        assert_eq!(ext("IMG_1.JPG", false), "JPG");
        assert_eq!(ext("a.tar.gz", false), "gz");
        assert_eq!(ext(".bashrc", false), "—");
        assert_eq!(ext("trailing.", false), "—");
        assert_eq!(ext("noext", false), "—");
        assert_eq!(ext("photos.v2", true), "—", "a folder has none");
    }

    /// The listing's Date Created and Permissions come from the metadata it reads anyway: an
    /// item's own mode (a link's is the link's) and its birth time where the disk keeps one.
    #[test]
    fn the_listing_reads_creation_time_and_permissions() {
        let d = std::env::temp_dir().join(format!("arbor-fs-extra-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("f.txt"), "x").unwrap();
        let v = list(d.clone(), Sort::new(SortKey::Name, false)).result.unwrap();
        let f = v.iter().find(|e| e.name == "f.txt").unwrap();
        let m = std::fs::symlink_metadata(d.join("f.txt")).unwrap();
        assert_eq!(f.mode, mode_of(&m));
        assert_ne!(f.mode, 0);
        let born = m.created().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64);
        assert_eq!(f.born_ms(), born, "the birth time, where the disk keeps one (0 where it doesn't)");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(d.join("f.txt"), std::fs::Permissions::from_mode(0o640)).unwrap();
            std::os::unix::fs::symlink(d.join("sub"), d.join("link")).unwrap();
            let v = list(d.clone(), Sort::new(SortKey::Name, false)).result.unwrap();
            let get = |n: &str| v.iter().find(|e| e.name == n).unwrap();
            assert_eq!(fmt_mode(get("f.txt")), "-rw-r-----");
            assert!(fmt_mode(get("sub")).starts_with('d'));
            assert!(fmt_mode(get("link")).starts_with('l'), "the link's own");
            assert!(get("link").is_dir, "and it still opens as a folder");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn free_space_is_read_for_the_volume() {
        let d = std::env::temp_dir();
        assert!(free_space(&d).is_some_and(|n| n > 0), "the temp folder's disk has room");
        assert_eq!(free_space(Path::new("/no/such/folder/anywhere")), None);
    }

    /// The columns cost a listing no memory: an entry is the size it was before them (its
    /// mode sits in padding; Date Created is its identity's birth time).
    #[test]
    fn an_entry_is_no_bigger_for_the_columns() {
        /// `Entry` as it was before the columns (256 bytes on 64-bit Linux and macOS, 272 on
        /// Windows, whose `OsString` is bigger).
        #[allow(dead_code)]
        struct Before {
            os: OsString,
            name: String,
            lower: String,
            key: String,
            is_dir: bool,
            is_link: bool,
            hidden: bool,
            size: u64,
            mtime_ms: i64,
            id: Option<Ident>,
            pending: Option<String>,
        }
        assert_eq!(std::mem::size_of::<Entry>(), std::mem::size_of::<Before>());
    }
}
