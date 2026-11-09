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
//!   operation, [`Mounts`](crate::mounts::Mounts)), is never entered or removed: the delete stops there and says another
//!   disk is mounted there. So does an item that is itself a mount point. On macOS each folder's
//!   volume is compared too (`mounts::volume`): the APFS volumes of one group share a device number
//!   (`/` and `/System/Volumes/Data`), and firmlinks (`/Users`, `/Applications`) lead from one
//!   to the other.
//! - **Identity:** each folder's opened handle must be the folder that was checked, and before
//!   it's removed its name must still lead to it; one moved or replaced meanwhile stops the
//!   delete there, untouched.
//! - **Windows:** a file or folder with the read-only attribute is deleted after clearing it
//!   (as Explorer does); the attribute is put back if the delete still fails. A directory link
//!   or junction is removed as itself, and a volume mounted in a folder is left alone.

use std::io;
use std::path::{Path, PathBuf};

use crate::mounts::{Fence, Mounts};

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
    /// The folder at `at` was moved or replaced while it was being deleted: it (or what's now
    /// at its name) was left alone, and nothing after it in this item was tried.
    Changed { at: PathBuf },
}

impl Halt {
    /// Where it stopped and why, in words (`at` as `shown` gives it).
    pub fn why(&self) -> Option<(&Path, String)> {
        match self {
            Halt::Cancelled => None,
            Halt::Failed { at, err } => Some((at, err.to_string())),
            Halt::Mounted { at } => Some((at, "another disk is mounted there".into())),
            Halt::Changed { at } => Some((at, "it was moved or replaced while it was being deleted".into())),
        }
    }
}

/// Removes `path` and everything in it, deepest first. Calls `go` with each item's path right
/// before removing it; `false` stops (a cancel). Adds one to `removed` for each file, link and
/// folder removed. Never enters or removes a mount point (`mounts`, or another device).
pub fn remove_tree(path: &Path, mounts: &Mounts, go: &mut dyn FnMut(&Path) -> bool, removed: &mut u64) -> Result<(), Halt> {
    imp::remove_tree(path, mounts, go, removed, false)
}

/// [`remove_tree`] for `name` in the open folder `dir`, which the caller has checked is the
/// folder it means (Empty Trash's `files/`): nothing is reached by its path. `path` is where it
/// is, for messages and the mount table.
#[cfg(unix)]
pub fn remove_in(
    dir: &std::os::fd::OwnedFd,
    name: &std::ffi::CStr,
    path: &Path,
    mounts: &Mounts,
    go: &mut dyn FnMut(&Path) -> bool,
    removed: &mut u64,
) -> Result<(), Halt> {
    imp::remove_in(dir, name, path, mounts, go, removed, false)
}

/// [`remove_tree`]'s walk with every check, removing nothing: where it would stop, and how many
/// items it would remove (tests, and the Mac's check that it stops at a volume boundary).
#[cfg(all(test, unix))]
pub fn dry_run(path: &Path, mounts: &Mounts) -> (Result<(), Halt>, u64) {
    let mut n = 0;
    let r = imp::remove_tree(path, mounts, &mut |_| true, &mut n, true);
    (r, n)
}

/// What a delete that stopped left behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Left {
    /// This many items (the item itself included).
    Exactly(u64),
    /// At least one, but this folder or item couldn't be read, so how many isn't known.
    Unknown(PathBuf),
}

/// How many items are left in `path` (itself included), not entering mount points: what a
/// delete that stopped left behind. A folder or item that can't be read makes the count
/// unknown rather than low. `None` when `go` says stop.
pub fn count_left(path: &Path, mounts: &Mounts, go: &mut dyn FnMut() -> bool) -> Option<Left> {
    let fence = Fence::new(path, mounts);
    fn walk(p: &Path, fence: &Fence, go: &mut dyn FnMut() -> bool) -> Option<Result<u64, PathBuf>> {
        if !go() {
            return None;
        }
        let m = match p.symlink_metadata() {
            Ok(m) => m,
            // Gone since it was listed: nothing left there.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Some(Ok(0)),
            Err(_) => return Some(Err(p.to_path_buf())),
        };
        let mut n = 1;
        if m.is_dir() && !fence.crosses(p, &m) {
            let Ok(entries) = std::fs::read_dir(p) else { return Some(Err(p.to_path_buf())) };
            for de in entries {
                let Ok(de) = de else { return Some(Err(p.to_path_buf())) };
                match walk(&de.path(), fence, go)? {
                    Ok(k) => n += k,
                    Err(at) => return Some(Err(at)),
                }
            }
        }
        Some(Ok(n))
    }
    walk(path, &fence, go).map(|r| r.map_or_else(Left::Unknown, Left::Exactly))
}

#[cfg(unix)]
mod imp {
    use std::ffi::{CStr, OsStr};
    use std::io;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use super::Halt;
    use crate::mounts::{Mounts, volume};

    use crate::dirfd::{fstat, is_dir, names, open_dir, same, stat_at};
    use crate::ffi::cstr;

    fn fail(at: &Path, err: io::Error) -> Halt {
        Halt::Failed { at: at.to_path_buf(), err }
    }

    /// What every folder in one delete is checked against.
    struct Walk<'a> {
        /// The item's device: a folder on another is a mount.
        dev: libc::dev_t,
        /// macOS: the item's volume; a folder on another is a mount or behind a firmlink.
        vol: Option<Vec<u8>>,
        /// Mount points inside the item, by canonical path (usually none).
        inside: Vec<PathBuf>,
        go: &'a mut dyn FnMut(&Path) -> bool,
        removed: &'a mut u64,
        /// Walk and check, but remove nothing (`dry_run`).
        dry: bool,
    }

    pub fn remove_tree(path: &Path, mounts: &Mounts, go: &mut dyn FnMut(&Path) -> bool, removed: &mut u64, dry: bool) -> Result<(), Halt> {
        let name = path.file_name().ok_or_else(|| fail(path, io::Error::from(io::ErrorKind::InvalidInput)))?;
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        // The parent is reached as the user reached it, links and all; only what's below it
        // is never followed.
        let dir = cstr(parent.as_os_str()).and_then(|p| open_dir(libc::AT_FDCWD, &p, true)).map_err(|e| fail(parent, e))?;
        let name = cstr(name).map_err(|e| fail(path, e))?;
        remove_in(&dir, &name, path, mounts, go, removed, dry)
    }

    /// [`remove_tree`] for `name` in folder `dir`, already open (and checked by the caller):
    /// `shown` is its path, for messages and the mount table.
    pub fn remove_in(
        dir: &OwnedFd,
        name: &CStr,
        path: &Path,
        mounts: &Mounts,
        go: &mut dyn FnMut(&Path) -> bool,
        removed: &mut u64,
        dry: bool,
    ) -> Result<(), Halt> {
        let parent = path.parent().unwrap_or(path);
        let st = stat_at(dir, name).map_err(|e| fail(path, e))?;
        // An item on another device than its folder is itself a mount point.
        let parent_dev = fstat(dir).map_err(|e| fail(parent, e))?.st_dev;
        let canon = crate::mounts::canonical(path);
        let inside = mounts.under(&canon);
        if is_dir(&st) && st.st_dev != parent_dev {
            return Err(Halt::Mounted { at: path.to_path_buf() });
        }
        // macOS: a folder on another volume than its folder (a mount point sharing the device
        // number, or a firmlink such as /Users) is one too.
        let mut vol = None;
        if is_dir(&st)
            && let Some(parent_vol) = volume::of_fd(dir.as_raw_fd())
            && let Ok(item) = open_dir(dir.as_raw_fd(), name, false)
        {
            vol = volume::of_fd(item.as_raw_fd());
            if vol.as_ref().is_some_and(|v| *v != parent_vol) {
                return Err(Halt::Mounted { at: path.to_path_buf() });
            }
        }
        let mut w = Walk { dev: st.st_dev, vol, inside, go, removed, dry };
        remove_at(dir, name, path, &canon, &mut w)
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
            // And a folder swapped in since is refused too.
            if !same(&fstat(&sub).map_err(|e| fail(shown, e))?, &st) {
                return Err(Halt::Changed { at: shown.to_path_buf() });
            }
            if w.vol.is_some() && volume::of_fd(sub.as_raw_fd()) != w.vol {
                return Err(Halt::Mounted { at: shown.to_path_buf() });
            }
            for child in names(&sub).map_err(|e| fail(shown, e))? {
                let n = OsStr::from_bytes(child.as_bytes());
                remove_at(&sub, &child, &shown.join(n), &canon.join(n), w)?;
            }
            // Its name must still lead to it: one moved aside meanwhile (and maybe replaced by
            // another folder) is left alone.
            match stat_at(dir, name) {
                Ok(now) if same(&now, &st) => {}
                _ => return Err(Halt::Changed { at: shown.to_path_buf() }),
            }
        }
        if w.dry {
            *w.removed += 1;
            return Ok(());
        }
        let flags = if is_dir { libc::AT_REMOVEDIR } else { 0 };
        // SAFETY: `name` is NUL-terminated and `dir` an open folder.
        if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flags) } != 0 {
            return Err(fail(shown, io::Error::last_os_error()));
        }
        *w.removed += 1;
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::fs::FileTypeExt;
    use std::path::{Path, PathBuf};

    use super::Halt;
    use crate::mounts::Mounts;

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

    /// (`_dry` is for Unix's `dry_run`; Windows is never asked for one.)
    pub fn remove_tree(path: &Path, mounts: &Mounts, go: &mut dyn FnMut(&Path) -> bool, removed: &mut u64, _dry: bool) -> Result<(), Halt> {
        // Windows lists no mounts (a volume mounted in a folder is a reparse point, checked
        // below); a list given in tests is honoured all the same.
        let canon = crate::mounts::canonical(path);
        let inside = mounts.under(&canon);
        rm(path, &canon, &inside, go, removed)
    }

    fn rm(path: &Path, canon: &Path, inside: &[PathBuf], go: &mut dyn FnMut(&Path) -> bool, removed: &mut u64) -> Result<(), Halt> {
        if !go(path) {
            return Err(Halt::Cancelled);
        }
        let m = path.symlink_metadata().map_err(|e| fail(path, e))?;
        let ft = m.file_type();
        if inside.iter().any(|i| i == canon) || ft.is_symlink_dir() && volume_mount_point(path) {
            return Err(Halt::Mounted { at: path.to_path_buf() });
        }
        // A link or junction isn't a folder here (`is_dir` is false for it): removed as itself.
        if ft.is_dir() {
            let rd = std::fs::read_dir(path).map_err(|e| fail(path, e))?;
            let children: Vec<PathBuf> = rd.map(|e| e.map(|e| e.path())).collect::<io::Result<_>>().map_err(|e| fail(path, e))?;
            for c in children {
                let name = c.file_name().map(PathBuf::from).unwrap_or_default();
                rm(&c, &canon.join(name), inside, go, removed)?;
            }
            // Its name must still lead to it (as far as Windows says without opening it).
            match path.symlink_metadata() {
                // The same folder (its object identity: its write time changed as its contents went).
                Ok(now) if now.file_type().is_dir() && crate::fs::ident(&now) == crate::fs::ident(&m) => {}
                _ => return Err(Halt::Changed { at: path.to_path_buf() }),
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
        let mounts = Mounts::of(&[&canon.join("t").join("mnt"), &canon.join("m2")]);
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
        // What's left is counted without entering the mount: t and mnt.
        assert_eq!(count_left(&d.join("t"), &mounts, &mut || true), Some(Left::Exactly(2)));
        let _ = fs::remove_dir_all(&d);
    }

    /// The review's case: a folder it can't read made the count low ("2 left" with 102 there).
    /// Now it's unknown, and says where.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_folder_makes_the_count_unknown_not_low() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid can't fail.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: permissions don't stop root");
            return;
        }
        let d = std::env::temp_dir().join(format!("arbor-delete-count-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("t/locked")).unwrap();
        for i in 0..100 {
            fs::write(d.join(format!("t/locked/{i}")), "x").unwrap();
        }
        fs::set_permissions(d.join("t/locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let got = count_left(&d.join("t"), &Mounts::default(), &mut || true);
        fs::set_permissions(d.join("t/locked"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(got, Some(Left::Unknown(d.join("t/locked"))));
        assert_eq!(count_left(&d.join("t"), &Mounts::default(), &mut || true), Some(Left::Exactly(102)));
        assert_eq!(count_left(&d.join("t"), &Mounts::default(), &mut || false), None, "a cancel stops the count");
        let _ = fs::remove_dir_all(&d);
    }

    /// The review's blocker: on macOS `/` and `/System/Volumes/Data` share a device number, so
    /// a delete walked into the Data volume, and through the `/Users` firmlink. A dry run (it
    /// removes nothing) now stops at each, and still walks a folder of the user's whole.
    #[cfg(target_os = "macos")]
    #[test]
    fn mac_volume_boundaries_stop_a_delete() {
        for root in ["/System/Volumes/Data", "/Users", "/Applications"] {
            let (r, n) = dry_run(Path::new(root), &Mounts::read());
            assert!(matches!(&r, Err(Halt::Mounted { at }) if at == Path::new(root)), "{root}: {r:?}");
            assert_eq!(n, 0, "{root}");
        }
        // Rooted above them: it stops at the first volume inside (after any empty folders of
        // the system volume it meets first), never past it.
        let (r, _) = dry_run(Path::new("/System/Volumes"), &Mounts::read());
        assert!(matches!(&r, Err(Halt::Mounted { at }) if at.parent() == Some(Path::new("/System/Volumes"))), "{r:?}");
        // The path-based fence agrees.
        let fence = Fence::new(Path::new("/System/Volumes"), &Mounts::read());
        let data = Path::new("/System/Volumes/Data");
        assert!(fence.crosses(data, &data.symlink_metadata().unwrap()));
        let fence = Fence::new(Path::new("/"), &Mounts::read());
        let users = Path::new("/Users");
        assert!(fence.crosses(users, &users.symlink_metadata().unwrap()), "the /Users firmlink is another volume");
    }

    /// A dry run walks with every check and removes nothing: a plain folder whole, and it stops
    /// at a mount point inside.
    #[cfg(unix)]
    #[test]
    fn a_dry_run_removes_nothing_and_stops_at_mounts() {
        let d = std::env::temp_dir().join(format!("arbor-delete-dry-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("a/b")).unwrap();
        fs::write(d.join("a/b/f"), "x").unwrap();
        let (r, n) = dry_run(&d, &Mounts::default());
        assert!(r.is_ok() && n == 4, "{r:?} {n}");
        assert!(d.join("a/b/f").exists(), "a dry run removes nothing");
        let canon = fs::canonicalize(&d).unwrap();
        let (r, _) = dry_run(&d, &Mounts::of(&[&canon.join("a/b")]));
        assert!(matches!(&r, Err(Halt::Mounted { at }) if at == &d.join("a/b")), "{r:?}");
        assert!(d.join("a/b/f").exists());
        let _ = fs::remove_dir_all(&d);
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

    /// A folder moved aside while its contents are deleted, and an empty one made at its name:
    /// the replacement is left alone (it used to be removed, reported as the folder).
    #[cfg(unix)]
    #[test]
    fn a_folder_moved_aside_midway_is_left_alone() {
        let d = tmp("aside");
        fs::create_dir_all(d.join("t")).unwrap();
        fs::write(d.join("t/child"), "x").unwrap();
        let (t, aside) = (d.join("t"), d.join("t-aside"));
        let mut n = 0;
        let r = remove_tree(
            &t,
            &Mounts::default(),
            &mut |at| {
                if at.ends_with("t/child") {
                    fs::rename(&t, &aside).unwrap();
                    fs::create_dir(&t).unwrap();
                    fs::write(t.join("theirs"), "another app's").unwrap();
                    fs::remove_file(t.join("theirs")).unwrap();
                }
                true
            },
            &mut n,
        );
        assert!(matches!(&r, Err(Halt::Changed { at }) if at == &d.join("t")), "{r:?}");
        assert!(d.join("t").is_dir(), "the replacement stays");
        assert!(d.join("t-aside").is_dir(), "the original, moved, stays");
        let _ = fs::remove_dir_all(&d);
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
