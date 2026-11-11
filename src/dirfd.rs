//! Unix: walking a tree through open folder descriptors, so nothing it reaches can be swapped
//! for a link and followed. A folder is opened relative to its parent's descriptor (or by
//! path) with `O_NOFOLLOW`, checked against the identity it had when it was listed
//! ([`same`]), and its entries are read and `lstat`ed relative to it. Permanent delete
//! (`delete.rs`) and Get Info's total (`info.rs`) walk this way.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// Opens folder `name` relative to `at` (`libc::AT_FDCWD`: a path). `follow`: a link as the
/// last component is followed; otherwise it's refused (`ELOOP`/`ENOTDIR`).
pub fn open_dir(at: RawFd, name: &CStr, follow: bool) -> io::Result<OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | if follow { 0 } else { libc::O_NOFOLLOW };
    // SAFETY: `name` is NUL-terminated; a non-negative result is a new descriptor we own.
    let fd = unsafe { libc::openat(at, name.as_ptr(), flags) };
    if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd) }) }
}

/// `lstat` of `name` in open folder `dir` (a link is described, not followed).
pub fn stat_at(dir: &OwnedFd, name: &CStr) -> io::Result<libc::stat> {
    // SAFETY: a zeroed stat is a valid out-buffer; `name` is NUL-terminated.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(dir.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

pub fn fstat(fd: &OwnedFd) -> io::Result<libc::stat> {
    // SAFETY: a zeroed stat is a valid out-buffer for an open descriptor.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

pub fn is_dir(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}

/// The same item: device and inode.
pub fn same(a: &libc::stat, b: &libc::stat) -> bool {
    (a.st_dev, a.st_ino) == (b.st_dev, b.st_ino)
}

/// Device and inode, as `MetadataExt` gives them (`dev_t` is i32 on macOS).
#[allow(clippy::unnecessary_cast)]
pub fn node(st: &libc::stat) -> (u64, u64) {
    (st.st_dev as u64, st.st_ino as u64)
}

/// The [`crate::fs::Ident`] of `name` in open folder `dir` (a link is described, not
/// followed): the same as `fs::ident_of` gives for it (Linux asks `statx`, as `std` does, for
/// the birth time, and where there's none, the file system's handle).
pub fn ident_at(dir: &OwnedFd, name: &CStr) -> io::Result<crate::fs::Ident> {
    let id = ident_raw(dir.as_raw_fd(), name, libc::AT_SYMLINK_NOFOLLOW)?;
    Ok(if id.wants_handle() { id.with_handle(handle_key(dir.as_raw_fd(), name, 0)) } else { id })
}

/// The [`crate::fs::Ident`] of the open file or folder `fd` itself, handle and all.
pub fn ident_of_fd(fd: &OwnedFd) -> io::Result<crate::fs::Ident> {
    #[cfg(target_os = "linux")]
    let (flags, handle_flags) = (libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW, libc::AT_EMPTY_PATH);
    #[cfg(not(target_os = "linux"))]
    let (flags, handle_flags) = (0, 0);
    let id = ident_raw(fd.as_raw_fd(), c"", flags)?;
    Ok(if id.wants_handle() { id.with_handle(handle_key(fd.as_raw_fd(), c"", handle_flags)) } else { id })
}

/// `statx` (Linux) or `fstatat`/`fstat` for `name` relative to `dir` with `flags`, as an
/// identity without a handle. An empty `name` is `dir` itself.
fn ident_raw(dir: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<crate::fs::Ident> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: a zeroed statx is a valid out-buffer; `name` is NUL-terminated.
        let mut x: libc::statx = unsafe { std::mem::zeroed() };
        let mask = libc::STATX_BASIC_STATS | libc::STATX_BTIME;
        let r = unsafe { libc::statx(dir, name.as_ptr(), flags, mask, &mut x) };
        if r == 0 {
            let t = |s: i64, n: u32| s as i128 * 1_000_000_000 + n as i128;
            let born = (x.stx_mask & libc::STATX_BTIME != 0).then(|| t(x.stx_btime.tv_sec, x.stx_btime.tv_nsec));
            let dev = libc::makedev(x.stx_dev_major, x.stx_dev_minor);
            let changed = t(x.stx_ctime.tv_sec, x.stx_ctime.tv_nsec);
            return Ok(crate::fs::Ident::of_parts(dev, x.stx_ino, u32::from(x.stx_mode), born, changed));
        }
        let e = io::Error::last_os_error();
        // No statx (an old kernel, a sandbox): what `std` falls back to, without a birth time.
        if !matches!(e.raw_os_error(), Some(libc::ENOSYS | libc::EPERM)) {
            return Err(e);
        }
    }
    // SAFETY: a zeroed stat is a valid out-buffer; `name` is NUL-terminated.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let r =
        if name.is_empty() { unsafe { libc::fstat(dir, &mut st) } } else { unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, flags) } };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    let (dev, ino) = node(&st);
    #[allow(clippy::unnecessary_cast)]
    let changed = st.st_ctime as i128 * 1_000_000_000 + st.st_ctime_nsec as i128;
    #[cfg(target_os = "macos")]
    let born = Some(st.st_birthtime as i128 * 1_000_000_000 + st.st_birthtime_nsec as i128);
    #[cfg(not(target_os = "macos"))]
    let born = None;
    #[allow(clippy::unnecessary_cast)]
    Ok(crate::fs::Ident::of_parts(dev, ino, st.st_mode as u32, born, changed))
}

/// Linux: the file system's handle for `name` in open folder `dir` (a link's own, not
/// followed): its inode and the inode's generation, so an item that got a freed inode back is
/// told from the one that had it. `None` where the file system gives none, and elsewhere.
pub fn handle_at(dir: &OwnedFd, name: &CStr) -> Option<Vec<u8>> {
    handle_raw(dir.as_raw_fd(), name, 0)
}

/// [`handle_at`] as an identity's part: the handle's type and a hash of its bytes (`flags`:
/// `AT_EMPTY_PATH` for `dir` itself; a link is never followed). `None` where there's none.
pub fn handle_key(dir: RawFd, name: &CStr, flags: libc::c_int) -> Option<(u64, u64)> {
    use std::hash::{Hash, Hasher};
    if crate::fs::pretend::no_handle() {
        return None;
    }
    let v = handle_raw(dir, name, flags)?;
    // `DefaultHasher::new` has fixed keys: the same handle hashes the same every time.
    let mut h = std::hash::DefaultHasher::new();
    v.hash(&mut h);
    let kind = i32::from_ne_bytes(v.get(..4)?.try_into().ok()?);
    Some((kind as u64, h.finish()))
}

fn handle_raw(dir: RawFd, name: &CStr, flags: libc::c_int) -> Option<Vec<u8>> {
    #[cfg(target_os = "linux")]
    {
        const MAX: usize = 128;
        #[repr(C)]
        struct Handle {
            bytes: libc::c_uint,
            kind: libc::c_int,
            data: [u8; MAX],
        }
        let mut h = Handle { bytes: MAX as libc::c_uint, kind: 0, data: [0; MAX] };
        let mut mount: libc::c_int = 0;
        // SAFETY: `h` starts as a `file_handle` with room for MAX bytes, as `bytes` says;
        // `name` is NUL-terminated. Without AT_SYMLINK_FOLLOW a link is described, not followed.
        let r = unsafe { libc::name_to_handle_at(dir, name.as_ptr(), (&raw mut h).cast(), &mut mount, flags) };
        if r != 0 {
            return None;
        }
        let mut v = h.kind.to_ne_bytes().to_vec();
        v.extend_from_slice(&h.data[..(h.bytes as usize).min(MAX)]);
        Some(v)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (dir, name, flags);
        None
    }
}

/// Where the link `name` in open folder `dir` points (`readlinkat`: no path re-resolved).
pub fn read_link_at(dir: &OwnedFd, name: &CStr) -> io::Result<std::path::PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut cap = 256;
    loop {
        let mut buf = vec![0u8; cap];
        // SAFETY: `name` is NUL-terminated and `buf` is as long as we say.
        let n = unsafe { libc::readlinkat(dir.as_raw_fd(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let n = n as usize;
        // Filled to the brim: it may have been cut short.
        if n < cap {
            buf.truncate(n);
            return Ok(std::ffi::OsString::from_vec(buf).into());
        }
        cap *= 4;
    }
}

/// Removes `name` from open folder `dir` (not a folder: `unlinkat` without `AT_REMOVEDIR`).
pub fn unlink_at(dir: &OwnedFd, name: &CStr) -> io::Result<()> {
    // SAFETY: `name` is NUL-terminated and `dir` an open folder.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// The names in open folder `dir`, without `.` and `..`, all at once (a delete, which removes
/// entries as it goes, reads the folder first). A read error is an error, never the end.
pub fn names(dir: &OwnedFd) -> io::Result<Vec<CString>> {
    Names::open(dir)?.collect()
}

/// The names in an open folder, one `readdir` at a time, without `.` and `..`: dropping it
/// stops reading (a cancel reads nothing more). `readdir` answers "nothing" both at the end
/// and on an error; `errno` tells them apart, so an error is yielded as one (then it ends)
/// rather than taken for the end of the folder.
pub struct Names {
    d: std::ptr::NonNull<libc::DIR>,
    done: bool,
    /// Tests: the folder's inode, for `hook`.
    #[cfg(test)]
    ino: u64,
}

impl Names {
    /// Starts reading folder `dir` (through a duplicate of its descriptor).
    pub fn open(dir: &OwnedFd) -> io::Result<Names> {
        // SAFETY: fdopendir takes over the duplicate on success; on failure it's closed here.
        unsafe {
            let fd = libc::dup(dir.as_raw_fd());
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let Some(d) = std::ptr::NonNull::new(libc::fdopendir(fd)) else {
                let e = io::Error::last_os_error();
                libc::close(fd);
                return Err(e);
            };
            // A duplicate shares its position with `dir`: start from the first entry.
            libc::rewinddir(d.as_ptr());
            #[allow(clippy::unnecessary_cast)]
            Ok(Names {
                d,
                done: false,
                #[cfg(test)]
                ino: fstat(dir).map_or(0, |st| st.st_ino as u64),
            })
        }
    }
}

impl Iterator for Names {
    type Item = io::Result<CString>;

    fn next(&mut self) -> Option<io::Result<CString>> {
        while !self.done {
            #[cfg(test)]
            if let Some(e) = hook::read(self.ino) {
                self.done = true;
                return Some(Err(e));
            }
            set_errno(0);
            // SAFETY: `d` is open until drop; each entry's name is copied before the next call.
            let e = unsafe { libc::readdir(self.d.as_ptr()) };
            if e.is_null() {
                self.done = true;
                let err = io::Error::last_os_error();
                return (err.raw_os_error().is_some_and(|n| n != 0)).then_some(Err(err));
            }
            // SAFETY: a non-null entry has a NUL-terminated name.
            let n = unsafe { CStr::from_ptr((*e).d_name.as_ptr()) };
            if n.to_bytes() != b"." && n.to_bytes() != b".." {
                return Some(Ok(n.to_owned()));
            }
        }
        None
    }
}

impl Drop for Names {
    fn drop(&mut self) {
        // SAFETY: opened by fdopendir, closed once.
        unsafe { libc::closedir(self.d.as_ptr()) };
    }
}

fn set_errno(v: libc::c_int) {
    // SAFETY: the calling thread's errno.
    unsafe {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            *libc::__errno_location() = v;
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            *libc::__error() = v;
        }
    }
}

/// Tests only: counts the `readdir` calls on a folder (by inode), and makes one fail after so
/// many (an `EIO` from a failing disk).
#[cfg(test)]
pub(crate) mod hook {
    use std::sync::Mutex;

    /// A folder's inode, the reads so far, and after how many one fails.
    pub(crate) static WATCH: Mutex<Vec<(u64, u64, Option<u64>)>> = Mutex::new(Vec::new());

    pub(crate) fn watch(dir: &std::path::Path, fail_after: Option<u64>) -> u64 {
        use std::os::unix::fs::MetadataExt;
        let ino = dir.metadata().unwrap().ino();
        WATCH.lock().unwrap().push((ino, 0, fail_after));
        ino
    }

    pub(crate) fn reads(ino: u64) -> u64 {
        WATCH.lock().unwrap().iter().find(|w| w.0 == ino).map_or(0, |w| w.1)
    }

    pub(super) fn read(ino: u64) -> Option<std::io::Error> {
        let mut w = WATCH.lock().unwrap();
        let w = w.iter_mut().find(|w| w.0 == ino)?;
        w.1 += 1;
        w.2.is_some_and(|n| w.1 > n).then(|| std::io::Error::from_raw_os_error(libc::EIO))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One read at a time: taking two names reads twice, not the whole folder; an error is
    /// an error (then the end), never a quiet end of the folder.
    #[test]
    fn names_stream_and_tell_errors_from_the_end() {
        let d = std::env::temp_dir().join(format!("arbor-dirfd-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for i in 0..20 {
            std::fs::write(d.join(format!("f{i}")), "x").unwrap();
        }
        let fd = open_dir(libc::AT_FDCWD, &crate::ffi::cstr(&d).unwrap(), false).unwrap();
        let ino = hook::watch(&d, None);
        let two: Vec<_> = Names::open(&fd).unwrap().take(2).collect();
        assert_eq!(two.len(), 2);
        assert!(hook::reads(ino) <= 4, "read {} times for two names (with . and ..)", hook::reads(ino));
        assert_eq!(names(&fd).unwrap().len(), 20);
        hook::WATCH.lock().unwrap().retain(|w| w.0 != ino);
        hook::watch(&d, Some(5));
        let all: Vec<io::Result<CString>> = Names::open(&fd).unwrap().collect();
        assert!(all.last().is_some_and(|r| r.as_ref().is_err_and(|e| e.raw_os_error() == Some(libc::EIO))), "{all:?}");
        assert!(all.len() <= 6, "five reads at most, then the error: {all:?}");
        assert!(names(&fd).is_err(), "and the collected form says so too");
        hook::WATCH.lock().unwrap().retain(|w| w.0 != ino);
        let _ = std::fs::remove_dir_all(&d);
    }
}
