//! Deleting permanently: an item and, for a folder, everything in it, never following a link (a
//! link is removed, not what it points to). Only `ops` calls it, for "Delete Permanently…",
//! after Arbor's one dialog has said it can't be undone.
//!
//! - **Unix:** each folder is opened relative to its parent's handle with `O_NOFOLLOW`, and its
//!   entries are removed with `unlinkat` relative to it, so a folder swapped for a link while the
//!   delete runs is refused, never followed. File modes don't matter to a delete (only the
//!   folder's), and Arbor doesn't change any: what can't be removed is reported.
//! - **Windows:** a file or folder with the read-only attribute is deleted after clearing it
//!   (as Explorer does); the attribute is put back if the delete still fails. A directory link
//!   or junction is removed as itself.

use std::io;
use std::path::{Path, PathBuf};

/// Why a delete stopped.
#[derive(Debug)]
pub enum Halt {
    /// Asked to stop: what was removed so far stays removed.
    Cancelled,
    /// `at` couldn't be removed (or read); nothing after it in this item was tried.
    Failed { at: PathBuf, err: io::Error },
}

/// Removes `path` and everything in it, deepest first. Calls `go` with each item's path right
/// before removing it; `false` stops (a cancel). Adds one to `removed` for each file, link and
/// folder removed.
pub fn remove_tree(path: &Path, go: &mut dyn FnMut(&Path) -> bool, removed: &mut u64) -> Result<(), Halt> {
    imp::remove_tree(path, go, removed)
}

#[cfg(unix)]
mod imp {
    use std::ffi::{CStr, CString, OsStr};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::Halt;

    fn cstr(s: &OsStr) -> io::Result<CString> {
        CString::new(s.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }

    fn fail(at: &Path, err: io::Error) -> Halt {
        Halt::Failed { at: at.to_path_buf(), err }
    }

    pub fn remove_tree(path: &Path, go: &mut dyn FnMut(&Path) -> bool, removed: &mut u64) -> Result<(), Halt> {
        let name = path.file_name().ok_or_else(|| fail(path, io::Error::from(io::ErrorKind::InvalidInput)))?;
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        // The parent is reached as the user reached it, links and all; only what's below it
        // is never followed.
        let dir = cstr(parent.as_os_str()).and_then(|p| open_dir(libc::AT_FDCWD, &p, true)).map_err(|e| fail(parent, e))?;
        let name = cstr(name).map_err(|e| fail(path, e))?;
        remove_at(&dir, &name, path, go, removed)
    }

    fn open_dir(at: RawFd, name: &CStr, follow: bool) -> io::Result<OwnedFd> {
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | if follow { 0 } else { libc::O_NOFOLLOW };
        // SAFETY: `name` is NUL-terminated; a non-negative result is a new descriptor we own.
        let fd = unsafe { libc::openat(at, name.as_ptr(), flags) };
        if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd) }) }
    }

    fn remove_at(dir: &OwnedFd, name: &CStr, shown: &Path, go: &mut dyn FnMut(&Path) -> bool, removed: &mut u64) -> Result<(), Halt> {
        if !go(shown) {
            return Err(Halt::Cancelled);
        }
        // SAFETY: a zeroed stat is a valid out-buffer; `name` is NUL-terminated.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(dir.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(fail(shown, io::Error::last_os_error()));
        }
        let is_dir = st.st_mode & libc::S_IFMT == libc::S_IFDIR;
        if is_dir {
            // O_NOFOLLOW: a link put here since the check above is refused, not followed.
            let sub = open_dir(dir.as_raw_fd(), name, false).map_err(|e| fail(shown, e))?;
            for child in names(&sub).map_err(|e| fail(shown, e))? {
                remove_at(&sub, &child, &shown.join(OsStr::from_bytes(child.as_bytes())), go, removed)?;
            }
        }
        let flags = if is_dir { libc::AT_REMOVEDIR } else { 0 };
        // SAFETY: `name` is NUL-terminated and `dir` an open folder.
        if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flags) } != 0 {
            return Err(fail(shown, io::Error::last_os_error()));
        }
        *removed += 1;
        Ok(())
    }

    /// The names in open folder `dir`, without `.` and `..`.
    fn names(dir: &OwnedFd) -> io::Result<Vec<CString>> {
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
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::fs::FileTypeExt;
    use std::path::{Path, PathBuf};

    use super::Halt;

    fn fail(at: &Path, err: io::Error) -> Halt {
        Halt::Failed { at: at.to_path_buf(), err }
    }

    pub fn remove_tree(path: &Path, go: &mut dyn FnMut(&Path) -> bool, removed: &mut u64) -> Result<(), Halt> {
        if !go(path) {
            return Err(Halt::Cancelled);
        }
        let m = path.symlink_metadata().map_err(|e| fail(path, e))?;
        let ft = m.file_type();
        // A link or junction isn't a folder here (`is_dir` is false for it): removed as itself.
        if ft.is_dir() {
            let rd = std::fs::read_dir(path).map_err(|e| fail(path, e))?;
            let children: Vec<PathBuf> = rd.map(|e| e.map(|e| e.path())).collect::<io::Result<_>>().map_err(|e| fail(path, e))?;
            for c in children {
                remove_tree(&c, go, removed)?;
            }
        }
        let as_dir = ft.is_dir() || ft.is_symlink_dir();
        remove_clearing_read_only(path, &m, as_dir).map_err(|e| fail(path, e))?;
        *removed += 1;
        Ok(())
    }

    fn remove(path: &Path, dir: bool) -> io::Result<()> {
        if dir { std::fs::remove_dir(path) } else { std::fs::remove_file(path) }
    }

    /// Removes `path`; if that's refused and it has the read-only attribute, clears it and
    /// tries again, putting it back if that fails too.
    // Windows only: `set_readonly(false)` clears the read-only attribute and nothing else (the
    // lint is about Unix, where it would make the file world-writable).
    #[allow(clippy::permissions_set_readonly_false)]
    fn remove_clearing_read_only(path: &Path, m: &std::fs::Metadata, dir: bool) -> io::Result<()> {
        match remove(path, dir) {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied && m.permissions().readonly() => {
                let mut p = m.permissions();
                p.set_readonly(false);
                std::fs::set_permissions(path, p).map_err(|_| e)?;
                remove(path, dir).inspect_err(|_| {
                    let _ = std::fs::set_permissions(path, m.permissions());
                })
            }
            r => r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-delete-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// A link to a folder; false where links can't be made (Windows without the right).
    fn link_dir(target: &Path, at: &Path) -> bool {
        #[cfg(unix)]
        let r = std::os::unix::fs::symlink(target, at);
        #[cfg(windows)]
        let r = std::os::windows::fs::symlink_dir(target, at);
        r.is_ok()
    }

    fn link_file(target: &Path, at: &Path) -> bool {
        #[cfg(unix)]
        let r = std::os::unix::fs::symlink(target, at);
        #[cfg(windows)]
        let r = std::os::windows::fs::symlink_file(target, at);
        r.is_ok()
    }

    fn all(p: &Path) -> Result<u64, Halt> {
        let mut n = 0;
        remove_tree(p, &mut |_| true, &mut n).map(|()| n)
    }

    /// A folder goes with everything in it; a link inside it goes, and what it points to (a
    /// folder and a file outside) stays whole.
    #[test]
    fn deletes_folders_whole_and_never_follows_links() {
        let d = tmp("links");
        fs::create_dir_all(d.join("doomed/sub/deeper")).unwrap();
        fs::write(d.join("doomed/a"), "x").unwrap();
        fs::write(d.join("doomed/sub/deeper/b"), "x").unwrap();
        fs::create_dir_all(d.join("keep/inner")).unwrap();
        fs::write(d.join("keep/inner/k"), "keep").unwrap();
        fs::write(d.join("keep.txt"), "keep").unwrap();
        let linked = link_dir(&d.join("keep"), &d.join("doomed/to-keep")) && link_file(&d.join("keep.txt"), &d.join("doomed/to-keep.txt"));
        let n = all(&d.join("doomed")).unwrap();
        assert!(!d.join("doomed").exists());
        assert_eq!(n, if linked { 7 } else { 5 });
        assert_eq!(fs::read_to_string(d.join("keep/inner/k")).unwrap(), "keep");
        assert_eq!(fs::read_to_string(d.join("keep.txt")).unwrap(), "keep");
        // A link given itself: the link goes, its target stays.
        if link_dir(&d.join("keep"), &d.join("link")) {
            all(&d.join("link")).unwrap();
            assert!(d.join("link").symlink_metadata().is_err() && d.join("keep/inner/k").exists());
        }
        let _ = fs::remove_dir_all(&d);
    }

    /// A read-only file: Unix deletes it as it is (a file's mode doesn't guard it; its folder's
    /// does), Windows after clearing the attribute.
    #[test]
    fn deletes_read_only_files() {
        let d = tmp("readonly");
        fs::create_dir(d.join("f")).unwrap();
        fs::write(d.join("f/ro"), "x").unwrap();
        let mut p = fs::metadata(d.join("f/ro")).unwrap().permissions();
        p.set_readonly(true);
        fs::set_permissions(d.join("f/ro"), p).unwrap();
        all(&d.join("f")).unwrap();
        assert!(!d.join("f").exists());
        let _ = fs::remove_dir_all(&d);
    }

    /// What can't be removed (a file in a folder this user can't change) stops that item with
    /// the reason, and what went before it stays gone.
    #[cfg(unix)]
    #[test]
    fn reports_what_it_cant_remove() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid can't fail.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: permissions don't stop root");
            return;
        }
        let d = tmp("locked");
        fs::create_dir_all(d.join("t/locked")).unwrap();
        fs::write(d.join("t/locked/x"), "x").unwrap();
        fs::set_permissions(d.join("t/locked"), fs::Permissions::from_mode(0o555)).unwrap();
        let r = all(&d.join("t"));
        assert!(
            matches!(&r, Err(Halt::Failed { at, err }) if at.ends_with("locked/x") && err.kind() == io::ErrorKind::PermissionDenied),
            "{r:?}"
        );
        assert!(d.join("t/locked/x").exists());
        fs::set_permissions(d.join("t/locked"), fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&d);
    }

    /// Stopping midway keeps what was removed removed and the rest where it was.
    #[test]
    fn stops_when_told() {
        let d = tmp("stop");
        fs::create_dir(d.join("t")).unwrap();
        for i in 0..10 {
            fs::write(d.join(format!("t/{i}")), "x").unwrap();
        }
        let mut seen = 0;
        let mut n = 0;
        let r = remove_tree(
            &d.join("t"),
            &mut |_| {
                seen += 1;
                seen <= 5
            },
            &mut n,
        );
        assert!(matches!(r, Err(Halt::Cancelled)));
        assert_eq!(n, 4, "four files; the folder itself stays");
        assert_eq!(fs::read_dir(d.join("t")).unwrap().count(), 6);
        let _ = fs::remove_dir_all(&d);
    }
}
