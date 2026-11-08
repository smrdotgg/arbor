//! Deleting permanently: an item and, for a folder, everything in it, never following a link (a
//! link is removed, not what it points to) and never crossing into another mounted disk. Only
//! `ops` calls it, for "Delete Permanently…", after Arbor's one dialog has said it can't be
//! undone.
//!
//! - **Unix:** each folder is opened relative to its parent's handle with `O_NOFOLLOW`, and its
//!   entries are removed with `unlinkat` relative to it, so a folder swapped for a link while the
//!   delete runs is refused, never followed. File modes don't matter to a delete (only the
//!   folder's), and Arbor doesn't change any: what can't be removed is reported.
//! - **Mounts:** a folder on another device than the item (a disk, a FUSE or network mount), or
//!   a mount point on the same one (a bind mount: Linux reads `/proc/self/mountinfo` once per
//!   operation, [`Mounts`]), is never entered or removed: the delete stops there and says another
//!   disk is mounted there. So does an item that is itself a mount point.
//! - **Windows:** a file or folder with the read-only attribute is deleted after clearing it
//!   (as Explorer does); the attribute is put back if the delete still fails. A directory link
//!   or junction is removed as itself, and a volume mounted in a folder is left alone.

use std::io;
use std::path::{Path, PathBuf};

/// Why a delete stopped.
#[derive(Debug)]
pub enum Halt {
    /// Asked to stop: what was removed so far stays removed.
    Cancelled,
    /// `at` couldn't be removed (or read); nothing after it in this item was tried.
    Failed { at: PathBuf, err: io::Error },
    /// Another disk is mounted at `at`: neither it nor anything in it was touched, and nothing
    /// after it in this item was tried.
    Mounted { at: PathBuf },
}

impl Halt {
    /// Where it stopped and why, in words (`at` as `shown` gives it).
    pub fn why(&self) -> Option<(&Path, String)> {
        match self {
            Halt::Cancelled => None,
            Halt::Failed { at, err } => Some((at, err.to_string())),
            Halt::Mounted { at } => Some((at, "another disk is mounted there".into())),
        }
    }
}

/// Where other file systems are mounted: read once per operation, on its worker. Only Linux
/// lists them (for bind mounts, which share their device); elsewhere every mount has a device
/// of its own, which the walk compares.
#[derive(Clone, Debug, Default)]
pub struct Mounts(Vec<PathBuf>);

impl Mounts {
    pub fn read() -> Mounts {
        #[cfg(target_os = "linux")]
        if let Ok(s) = std::fs::read_to_string("/proc/self/mountinfo") {
            return Mounts::parse(&s);
        }
        Mounts::default()
    }

    /// `/proc/self/mountinfo`'s mount points (the fifth field, with its octal escapes).
    #[cfg(any(target_os = "linux", test))]
    pub fn parse(s: &str) -> Mounts {
        Mounts(s.lines().filter_map(|l| l.split(' ').nth(4)).map(unescape).collect())
    }

    /// Exactly these mount points (tests).
    #[cfg(test)]
    pub fn of(points: &[&Path]) -> Mounts {
        Mounts(points.iter().map(|p| p.to_path_buf()).collect())
    }

    /// The mount points at or below `root` (a canonical path).
    fn under(&self, root: &Path) -> Vec<PathBuf> {
        self.0.iter().filter(|m| m.starts_with(root)).cloned().collect()
    }
}

/// `\040` and the like back to their bytes.
#[cfg(any(target_os = "linux", test))]
fn unescape(s: &str) -> PathBuf {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    #[cfg(unix)]
    return PathBuf::from(<std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(out));
    #[cfg(not(unix))]
    PathBuf::from(String::from_utf8_lossy(&out).into_owned())
}

/// The mount boundaries of one tree: its device, and the mount points inside it (by canonical
/// path). For the other recursive removals (a cross-disk move's originals), which go by path.
pub struct Fence {
    root: PathBuf,
    canon: PathBuf,
    #[cfg(unix)]
    dev: Option<u64>,
    inside: Vec<PathBuf>,
}

impl Fence {
    pub fn new(root: &Path, mounts: &Mounts) -> Fence {
        let canon = canonical(root);
        #[cfg(unix)]
        let dev = root.symlink_metadata().ok().map(|m| std::os::unix::fs::MetadataExt::dev(&m));
        Fence {
            root: root.to_path_buf(),
            inside: mounts.under(&canon),
            canon,
            #[cfg(unix)]
            dev,
        }
    }

    /// Whether `p` (in the tree, with its own `lstat`) is a mount point or on another disk.
    pub fn crosses(&self, p: &Path, m: &std::fs::Metadata) -> bool {
        #[cfg(unix)]
        if self.dev.is_some_and(|d| d != std::os::unix::fs::MetadataExt::dev(m)) {
            return true;
        }
        #[cfg(not(unix))]
        let _ = m;
        let rel = p.strip_prefix(&self.root).unwrap_or(p);
        !self.inside.is_empty() && self.inside.iter().any(|i| self.canon.join(rel).starts_with(i))
    }
}

/// `p` with its parent's links resolved (its own name kept as it is, since it may be a link).
fn canonical(p: &Path) -> PathBuf {
    match (p.parent(), p.file_name()) {
        (Some(d), Some(n)) => {
            std::fs::canonicalize(if d.as_os_str().is_empty() { Path::new(".") } else { d }).map_or_else(|_| p.into(), |d| d.join(n))
        }
        _ => p.to_path_buf(),
    }
}

/// Removes `path` and everything in it, deepest first. Calls `go` with each item's path right
/// before removing it; `false` stops (a cancel). Adds one to `removed` for each file, link and
/// folder removed. Never enters or removes a mount point (`mounts`, or another device).
pub fn remove_tree(path: &Path, mounts: &Mounts, go: &mut dyn FnMut(&Path) -> bool, removed: &mut u64) -> Result<(), Halt> {
    imp::remove_tree(path, mounts, go, removed)
}

#[cfg(unix)]
mod imp {
    use std::ffi::{CStr, CString, OsStr};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use super::{Halt, Mounts};

    fn cstr(s: &OsStr) -> io::Result<CString> {
        CString::new(s.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }

    fn fail(at: &Path, err: io::Error) -> Halt {
        Halt::Failed { at: at.to_path_buf(), err }
    }

    /// What every folder in one delete is checked against.
    struct Walk<'a> {
        /// The item's device: a folder on another is a mount.
        dev: libc::dev_t,
        /// Mount points inside the item, by canonical path (usually none).
        inside: Vec<PathBuf>,
        go: &'a mut dyn FnMut(&Path) -> bool,
        removed: &'a mut u64,
    }

    pub fn remove_tree(path: &Path, mounts: &Mounts, go: &mut dyn FnMut(&Path) -> bool, removed: &mut u64) -> Result<(), Halt> {
        let name = path.file_name().ok_or_else(|| fail(path, io::Error::from(io::ErrorKind::InvalidInput)))?;
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        // The parent is reached as the user reached it, links and all; only what's below it
        // is never followed.
        let dir = cstr(parent.as_os_str()).and_then(|p| open_dir(libc::AT_FDCWD, &p, true)).map_err(|e| fail(parent, e))?;
        let name = cstr(name).map_err(|e| fail(path, e))?;
        let st = stat_at(&dir, &name).map_err(|e| fail(path, e))?;
        // An item on another device than its folder is itself a mount point.
        let parent_dev = fstat(&dir).map_err(|e| fail(parent, e))?.st_dev;
        let canon = super::canonical(path);
        let inside = mounts.under(&canon);
        if is_dir(&st) && st.st_dev != parent_dev {
            return Err(Halt::Mounted { at: path.to_path_buf() });
        }
        let mut w = Walk { dev: st.st_dev, inside, go, removed };
        remove_at(&dir, &name, path, &canon, &mut w)
    }

    fn open_dir(at: RawFd, name: &CStr, follow: bool) -> io::Result<OwnedFd> {
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | if follow { 0 } else { libc::O_NOFOLLOW };
        // SAFETY: `name` is NUL-terminated; a non-negative result is a new descriptor we own.
        let fd = unsafe { libc::openat(at, name.as_ptr(), flags) };
        if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd) }) }
    }

    fn stat_at(dir: &OwnedFd, name: &CStr) -> io::Result<libc::stat> {
        // SAFETY: a zeroed stat is a valid out-buffer; `name` is NUL-terminated.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(dir.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st)
    }

    fn fstat(fd: &OwnedFd) -> io::Result<libc::stat> {
        // SAFETY: a zeroed stat is a valid out-buffer for an open descriptor.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st)
    }

    fn is_dir(st: &libc::stat) -> bool {
        st.st_mode & libc::S_IFMT == libc::S_IFDIR
    }

    fn remove_at(dir: &OwnedFd, name: &CStr, shown: &Path, canon: &Path, w: &mut Walk) -> Result<(), Halt> {
        if !(w.go)(shown) {
            return Err(Halt::Cancelled);
        }
        let st = stat_at(dir, name).map_err(|e| fail(shown, e))?;
        // A bind mount (same device), or anything mounted over a file.
        if !w.inside.is_empty() && w.inside.iter().any(|m| m == canon) {
            return Err(Halt::Mounted { at: shown.to_path_buf() });
        }
        let is_dir = is_dir(&st);
        if is_dir {
            if st.st_dev != w.dev {
                return Err(Halt::Mounted { at: shown.to_path_buf() });
            }
            // O_NOFOLLOW: a link put here since the check above is refused, not followed.
            let sub = open_dir(dir.as_raw_fd(), name, false).map_err(|e| fail(shown, e))?;
            for child in names(&sub).map_err(|e| fail(shown, e))? {
                let n = OsStr::from_bytes(child.as_bytes());
                remove_at(&sub, &child, &shown.join(n), &canon.join(n), w)?;
            }
        }
        let flags = if is_dir { libc::AT_REMOVEDIR } else { 0 };
        // SAFETY: `name` is NUL-terminated and `dir` an open folder.
        if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flags) } != 0 {
            return Err(fail(shown, io::Error::last_os_error()));
        }
        *w.removed += 1;
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

    use super::{Halt, Mounts};

    fn fail(at: &Path, err: io::Error) -> Halt {
        Halt::Failed { at: at.to_path_buf(), err }
    }

    /// Whether `path` (a folder link) is a volume mounted in a folder, which the shell resolves
    /// to a volume name; a junction or symbolic link doesn't.
    fn volume_mount_point(path: &Path) -> bool {
        use std::os::windows::ffi::OsStrExt;
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.last() != Some(&(b'\\' as u16)) {
            wide.push(b'\\' as u16);
        }
        wide.push(0);
        let mut name = [0u16; 64];
        // SAFETY: `wide` is NUL-terminated and `name` is as long as we say.
        unsafe {
            windows_sys::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW(wide.as_ptr(), name.as_mut_ptr(), name.len() as u32)
                != 0
        }
    }

    pub fn remove_tree(path: &Path, _mounts: &Mounts, go: &mut dyn FnMut(&Path) -> bool, removed: &mut u64) -> Result<(), Halt> {
        if !go(path) {
            return Err(Halt::Cancelled);
        }
        let m = path.symlink_metadata().map_err(|e| fail(path, e))?;
        let ft = m.file_type();
        if ft.is_symlink_dir() && volume_mount_point(path) {
            return Err(Halt::Mounted { at: path.to_path_buf() });
        }
        // A link or junction isn't a folder here (`is_dir` is false for it): removed as itself.
        if ft.is_dir() {
            let rd = std::fs::read_dir(path).map_err(|e| fail(path, e))?;
            let children: Vec<PathBuf> = rd.map(|e| e.map(|e| e.path())).collect::<io::Result<_>>().map_err(|e| fail(path, e))?;
            for c in children {
                remove_tree(&c, _mounts, go, removed)?;
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
        remove_tree(p, &Mounts::default(), &mut |_| true, &mut n).map(|()| n)
    }

    /// A folder something is mounted on (here, by the mount table the delete is given) is never
    /// entered or removed: the delete stops there and names it, and what's in it stays. So is an
    /// item that's itself a mount point.
    #[test]
    fn never_enters_a_mount_point() {
        let d = tmp("mounts");
        fs::create_dir_all(d.join("t/mnt/deep")).unwrap();
        fs::write(d.join("t/mnt/deep/precious"), "keep").unwrap();
        fs::write(d.join("t/mnt/precious"), "keep").unwrap();
        let canon = fs::canonicalize(&d).unwrap();
        let mounts = Mounts::of(&[&canon.join("t/mnt"), &canon.join("m2")]);
        let mut n = 0;
        let r = remove_tree(&d.join("t"), &mounts, &mut |_| true, &mut n);
        assert!(matches!(&r, Err(Halt::Mounted { at }) if at == &d.join("t/mnt")), "{r:?}");
        assert_eq!(fs::read_to_string(d.join("t/mnt/precious")).unwrap(), "keep");
        assert_eq!(fs::read_to_string(d.join("t/mnt/deep/precious")).unwrap(), "keep");
        assert_eq!(r.unwrap_err().why().unwrap().1, "another disk is mounted there");
        // The item itself.
        fs::create_dir(d.join("m2")).unwrap();
        fs::write(d.join("m2/precious"), "keep").unwrap();
        let r = remove_tree(&d.join("m2"), &mounts, &mut |_| true, &mut n);
        assert!(matches!(&r, Err(Halt::Mounted { at }) if at == &d.join("m2")), "{r:?}");
        assert!(d.join("m2/precious").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn reads_the_mount_table() {
        let m = Mounts::parse(
            "22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/root rw\n\
             36 22 0:31 / /media/my\\040disk rw,nosuid - fuse.sshfs host: rw\n\
             37 22 0:32 /sub /srv/bind rw - ext4 /dev/root rw\n",
        );
        assert_eq!(m.0, [PathBuf::from("/"), PathBuf::from("/media/my disk"), PathBuf::from("/srv/bind")]);
        assert_eq!(m.under(Path::new("/media")), [PathBuf::from("/media/my disk")]);
        assert!(m.under(Path::new("/srv/bin")).is_empty(), "a path prefix isn't a parent");
    }

    /// Deletes `ARBOR_DELETE_PROBE` for real, with the system's mount table (or none, with
    /// `ARBOR_DELETE_PROBE_DEV_ONLY`, so only the device check guards it), and prints what
    /// happened: for checking against a real mount by hand.
    #[test]
    #[ignore = "deletes the folder ARBOR_DELETE_PROBE names"]
    fn delete_probe() {
        let Some(p) = std::env::var_os("ARBOR_DELETE_PROBE") else { return };
        let mounts = if std::env::var_os("ARBOR_DELETE_PROBE_DEV_ONLY").is_some() { Mounts::default() } else { Mounts::read() };
        let mut n = 0;
        let r = remove_tree(
            Path::new(&p),
            &mounts,
            &mut |at| {
                eprintln!("visit {}", at.display());
                true
            },
            &mut n,
        );
        eprintln!("result={r:?} removed={n}");
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
            &Mounts::default(),
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
