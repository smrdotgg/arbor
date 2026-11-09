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
        assert!(all.len() <= 5);
        assert!(names(&fd).is_err(), "and the collected form says so too");
        hook::WATCH.lock().unwrap().retain(|w| w.0 != ino);
        let _ = std::fs::remove_dir_all(&d);
    }
}
