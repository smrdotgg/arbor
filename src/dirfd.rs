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

/// The names in open folder `dir`, without `.` and `..`.
pub fn names(dir: &OwnedFd) -> io::Result<Vec<CString>> {
    // SAFETY: fdopendir takes over the duplicate, and closedir closes it; each entry's name
    // is read before the next readdir.
    unsafe {
        let fd = libc::dup(dir.as_raw_fd());
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let d = libc::fdopendir(fd);
        if d.is_null() {
            let e = io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        let mut out = Vec::new();
        loop {
            let e = libc::readdir(d);
            if e.is_null() {
                break;
            }
            let n = CStr::from_ptr((*e).d_name.as_ptr());
            if n.to_bytes() != b"." && n.to_bytes() != b".." {
                out.push(n.to_owned());
            }
        }
        libc::closedir(d);
        Ok(out)
    }
}
