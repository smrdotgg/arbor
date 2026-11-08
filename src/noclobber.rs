//! Filesystem primitives that never replace an existing item (NORTHSTAR Safety #4). Every move,
//! rename and copy in Arbor goes through these, so a name taken between a check and the act
//! fails with `ErrorKind::AlreadyExists` instead of destroying what took it.
//!
//! - Moves and renames are atomic "rename unless the target exists": `renameat2` with
//!   `RENAME_NOREPLACE` on Linux, `renamex_np` with `RENAME_EXCL` on macOS, `MoveFileExW`
//!   without `MOVEFILE_REPLACE_EXISTING` on Windows. A disk that doesn't support the no-replace
//!   flag gets a fallback that still can't replace anything, or refuses (see
//!   `rename_fallback`). A case-only rename takes two of these steps through a temporary name
//!   (see [`rename`]). No plain, replacing `rename` is ever called.
//! - File copies create the destination exclusively: `clonefile` on macOS (fails if it exists),
//!   `CopyFileW` with "fail if exists" on Windows, `O_CREAT | O_EXCL` elsewhere.
//!   Folders are made with `create_dir` and links with `symlink`, which fail if the name is taken.

use std::io::{self, ErrorKind};
use std::path::Path;

/// Moves `from` to `to` on the same disk, failing with `AlreadyExists` if anything is at `to`
/// (and with the platform's cross-device error across disks). Nothing here ever calls a rename
/// that can replace.
///
/// When `to` names `from` itself (a case-only rename like `a.txt` → `A.txt` on a
/// case-insensitive disk such as APFS or NTFS, where the no-replace rename may see `to` as
/// taken), it goes in two no-replace steps through a temporary sibling ([`via_temp`]).
pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    match rename_noreplace(from, to) {
        Err(e) if e.kind() == ErrorKind::AlreadyExists && same_item(from, to) => via_temp(from, to),
        r => r,
    }
}

/// The no-replace rename, or on a disk without one the fallback that still can't replace.
#[cfg(unix)]
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    match rename_excl(from, to) {
        Err(e) if unsupported(&e) => rename_fallback(from, to),
        r => r,
    }
}

#[cfg(not(unix))]
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    rename_excl(from, to)
}

/// A rename where `to` already finds `from` (another spelling of its name, or another hard
/// link to it), in two no-replace steps: `from` → `.arbor-<random>-<name>` beside it → `to`.
/// Neither step can replace anything. Once `from` has its temporary name, a case-insensitive
/// disk sees `to` as free; a hard link at `to` (or anything that took `to` in between) still
/// makes the second step fail. Then the item goes back to `from`, again without replacing; if
/// even that name was taken meanwhile, it stays under the temporary name and the error says so.
///
/// Not crash-atomic: a crash between the two steps leaves the item under the temporary name in
/// the same folder (hidden on Unix). It's out of sight, but nothing is lost or replaced.
fn via_temp(from: &Path, to: &Path) -> io::Result<()> {
    let tmp = temp_sibling(from)?;
    rename_noreplace(from, &tmp)?;
    #[cfg(test)]
    tests::between();
    let Err(e) = rename_noreplace(&tmp, to) else { return Ok(()) };
    match rename_noreplace(&tmp, from) {
        Ok(()) => Err(e),
        Err(back) => Err(io::Error::other(format!(
            "couldn't rename it ({e}), nor give back its old name ({back}), so it's now named “{}” in the same folder",
            tmp.file_name().unwrap_or_default().to_string_lossy()
        ))),
    }
}

/// An unused-looking name beside `p` for [`via_temp`]: `.arbor-<64 random bits>-<start of its
/// name>`, kept short so a long name can't push it past the disk's name limit.
fn temp_sibling(p: &Path) -> io::Result<std::path::PathBuf> {
    use std::hash::BuildHasher;
    let name = p.file_name().ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "no file name"))?;
    // `RandomState` is seeded randomly per process; the clock varies it per call.
    let r = std::collections::hash_map::RandomState::new().hash_one((std::process::id(), std::time::SystemTime::now()));
    let start: String = name.to_string_lossy().chars().take(40).collect();
    Ok(p.with_file_name(format!(".arbor-{r:016x}-{start}")))
}

/// Whether `a` and `b` are the same item on disk (both exist and share an identity).
pub fn same_item(a: &Path, b: &Path) -> bool {
    match (a.symlink_metadata(), b.symlink_metadata()) {
        (Ok(ma), Ok(mb)) => file_id(a, &ma).is_some_and(|id| Some(id) == file_id(b, &mb)),
        _ => false,
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_excl(from: &Path, to: &Path) -> io::Result<()> {
    let (f, t) = (cstr(from)?, cstr(to)?);
    // The raw syscall rather than glibc's wrapper, which needs glibc 2.28.
    let r = unsafe { libc::syscall(libc::SYS_renameat2, libc::AT_FDCWD, f.as_ptr(), libc::AT_FDCWD, t.as_ptr(), libc::RENAME_NOREPLACE) };
    if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(target_vendor = "apple")]
fn rename_excl(from: &Path, to: &Path) -> io::Result<()> {
    let (f, t) = (cstr(from)?, cstr(to)?);
    let r = unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL) };
    if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Other Unixes: no atomic no-replace rename known here, so always the fallback.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android", target_vendor = "apple"))))]
fn rename_excl(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::ENOTSUP))
}

#[cfg(windows)]
fn rename_excl(from: &Path, to: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let (f, t) = (wide(from), wide(to));
    // No MOVEFILE_REPLACE_EXISTING: fails with ERROR_ALREADY_EXISTS if `to` exists. No
    // MOVEFILE_COPY_ALLOWED: across volumes it fails with ERROR_NOT_SAME_DEVICE, and the caller
    // copies with its own no-overwrite rules.
    if unsafe { MoveFileExW(f.as_ptr(), t.as_ptr(), 0) } != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// The disk (or kernel) doesn't support the no-replace flag: some network and FUSE filesystems
/// answer `EINVAL`, kernels before 3.15 `ENOSYS`.
#[cfg(unix)]
fn unsupported(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP)) || e.raw_os_error() == Some(libc::EOPNOTSUPP)
}

/// A move that still can't replace anything, for disks without a no-replace rename:
///
/// - Files and links: `link` (fails if `to` exists, and never follows a link), then remove the
///   old name. A disk without hard links can't move this way safely, so the move fails.
///   Not crash-atomic: a crash between the two leaves both names on the one file, a duplicate
///   name rather than lost data.
/// - Folders: refused. A folder can't be hard-linked, and every other way through ends in a
///   plain `rename`, which replaces an empty folder at `to` (reserving `to` first doesn't help:
///   another app can swap the reservation for its own empty folder in between).
#[cfg(unix)]
fn rename_fallback(from: &Path, to: &Path) -> io::Result<()> {
    let m = from.symlink_metadata()?;
    if m.is_dir() {
        return Err(io::Error::new(
            ErrorKind::Unsupported,
            "this disk can't move or rename a folder without risking replacing another one, so Arbor won't",
        ));
    }
    match std::fs::hard_link(from, to) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists || is_cross_device(&e) => return Err(e),
        Err(e) => return Err(io::Error::new(e.kind(), format!("this disk can't move it without risking an overwrite ({e})"))),
    }
    // Both names are the same file now; taking `to` away again (while it's still that file)
    // restores where it started.
    std::fs::remove_file(from).inspect_err(|_| {
        if same_item(from, to) {
            let _ = std::fs::remove_file(to);
        }
    })
}

/// Whether `e` means "that's on another disk".
pub fn is_cross_device(e: &io::Error) -> bool {
    e.kind() == ErrorKind::CrossesDevices || (cfg!(unix) && e.raw_os_error() == Some(18)) || (cfg!(windows) && e.raw_os_error() == Some(17))
}

/// Copies the file (or its clone) at `from` to a new file `to`, with its permissions. Fails with
/// `AlreadyExists` if anything is at `to`; a destination it made itself is removed on failure.
#[cfg(target_vendor = "apple")]
pub fn copy_file(from: &Path, to: &Path) -> io::Result<()> {
    // `<sys/clonefile.h>`; not in the libc crate.
    const CLONE_NOFOLLOW: u32 = 0x0001;
    // On APFS a clone: instant and no extra space. Fails if `to` exists.
    let (f, t) = (cstr(from)?, cstr(to)?);
    if unsafe { libc::clonefile(f.as_ptr(), t.as_ptr(), CLONE_NOFOLLOW) } == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        // Not APFS, or another volume: copy the bytes.
        Some(libc::ENOTSUP | libc::EXDEV | libc::EINVAL) => copy_bytes(from, to),
        _ => Err(e),
    }
}

#[cfg(all(unix, not(target_vendor = "apple")))]
pub fn copy_file(from: &Path, to: &Path) -> io::Result<()> {
    copy_bytes(from, to)
}

#[cfg(windows)]
pub fn copy_file(from: &Path, to: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::CopyFileW;
    let (f, t) = (wide(from), wide(to));
    // bFailIfExists: fails with ERROR_FILE_EXISTS rather than replacing. Copies attributes and
    // alternate streams like Explorer.
    if unsafe { CopyFileW(f.as_ptr(), t.as_ptr(), 1) } != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// `O_CREAT | O_EXCL`, then the bytes (`copy_file_range` on Linux, which clones on Btrfs and XFS).
#[cfg(unix)]
fn copy_bytes(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut src = std::fs::File::open(from)?;
    let perm = src.metadata()?.permissions();
    let mut dst = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(to)?;
    let done = io::copy(&mut src, &mut dst).and_then(|_| dst.set_permissions(perm));
    if done.is_err() {
        // Only the file this made: if another app replaced it at `to` meanwhile, that stays.
        use std::os::unix::fs::MetadataExt;
        let ours = dst.metadata().ok().map(|m| (m.dev(), m.ino()));
        drop(dst);
        if ours.is_some() && to.symlink_metadata().ok().map(|m| (m.dev(), m.ino())) == ours {
            let _ = std::fs::remove_file(to);
        }
    }
    done
}

/// The item's identity on its disk: device and inode on Unix; volume serial number and file
/// index on Windows (`GetFileInformationByHandle`), which stays with the file through renames
/// and changes when it's replaced.
#[cfg(unix)]
pub fn file_id(_: &Path, m: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((m.dev(), m.ino()))
}

/// Opens the item itself (not a link's target) without reading or locking it.
#[cfg(windows)]
pub fn file_id(path: &Path, _: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, GetFileInformationByHandle,
    };
    let f = std::fs::OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .ok()?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
        return None;
    }
    Some((u64::from(info.dwVolumeSerialNumber), (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)))
}

#[cfg(not(any(unix, windows)))]
pub fn file_id(_: &Path, _: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

#[cfg(unix)]
fn cstr(p: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::new(ErrorKind::InvalidInput, "name contains a NUL byte"))
}

#[cfg(windows)]
fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-noclobber-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn rename_refuses_a_taken_name() {
        let d = tmp("ren");
        fs::write(d.join("a"), "a").unwrap();
        fs::write(d.join("b"), "b").unwrap();
        assert_eq!(rename(&d.join("a"), &d.join("b")).unwrap_err().kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(d.join("b")).unwrap(), "b");
        // Folders too, including an empty one (which plain `rename` replaces on Unix).
        fs::create_dir(d.join("f")).unwrap();
        fs::create_dir(d.join("empty")).unwrap();
        assert_eq!(rename(&d.join("f"), &d.join("empty")).unwrap_err().kind(), ErrorKind::AlreadyExists);
        assert!(d.join("f").is_dir() && d.join("empty").is_dir());
        rename(&d.join("a"), &d.join("c")).unwrap();
        assert_eq!(fs::read_to_string(d.join("c")).unwrap(), "a");
    }

    #[cfg(unix)]
    #[test]
    fn fallback_refuses_a_taken_name() {
        let d = tmp("fallback");
        fs::write(d.join("a"), "a").unwrap();
        fs::write(d.join("b"), "b").unwrap();
        fs::create_dir(d.join("f")).unwrap();
        fs::write(d.join("f/x"), "x").unwrap();
        fs::create_dir(d.join("g")).unwrap();
        assert_eq!(rename_fallback(&d.join("a"), &d.join("b")).unwrap_err().kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(d.join("b")).unwrap(), "b");
        rename_fallback(&d.join("a"), &d.join("c")).unwrap();
        assert!(!d.join("a").exists());
        assert_eq!(fs::read_to_string(d.join("c")).unwrap(), "a");
        // Folders are refused, taken name or not: the only way through ends in a plain rename,
        // which replaces an empty folder (another app's, swapped in for a reservation).
        for to in ["g", "b", "h"] {
            assert_eq!(rename_fallback(&d.join("f"), &d.join(to)).unwrap_err().kind(), ErrorKind::Unsupported);
        }
        assert_eq!(fs::read_to_string(d.join("f/x")).unwrap(), "x");
        assert!(d.join("g").is_dir() && fs::read_dir(d.join("g")).unwrap().next().is_none());
        assert!(!d.join("h").exists());
    }

    /// Runs `f` between the two steps of the next [`via_temp`] on this thread.
    fn when_between(f: impl FnOnce() + 'static) {
        BETWEEN.with_borrow_mut(|b| *b = Some(Box::new(f)));
    }

    thread_local! {
        static BETWEEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    }

    pub(super) fn between() {
        if let Some(f) = BETWEEN.with_borrow_mut(Option::take) {
            f();
        }
    }

    fn names(d: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    /// A case-only rename lands under the new spelling, on a case-insensitive disk (macOS and
    /// Windows CI) as on a case-sensitive one, and leaves no temporary name behind.
    #[test]
    fn case_only_rename() {
        let d = tmp("case");
        fs::write(d.join("a.txt"), "mine").unwrap();
        fs::create_dir(d.join("f")).unwrap();
        rename(&d.join("a.txt"), &d.join("A.txt")).unwrap();
        rename(&d.join("f"), &d.join("F")).unwrap();
        assert_eq!(names(&d), ["A.txt", "F"]);
        assert_eq!(fs::read_to_string(d.join("A.txt")).unwrap(), "mine");
    }

    /// Two hard links are one item under two names, the same escape a case-only rename takes.
    /// Renaming one onto the other is refused and both stay (a plain rename there did nothing
    /// and claimed success). Unix only: on Windows the no-replace `MoveFileExW` itself renames
    /// one link onto another of the same file (as it does a case-only rename), so the two-step
    /// path isn't reached there; see `rename_onto_another_hard_link_loses_nothing`.
    #[cfg(unix)]
    #[test]
    fn rename_onto_another_hard_link_is_refused() {
        let d = tmp("alias");
        fs::write(d.join("a"), "mine").unwrap();
        fs::hard_link(d.join("a"), d.join("b")).unwrap();
        assert_eq!(rename(&d.join("a"), &d.join("b")).unwrap_err().kind(), ErrorKind::AlreadyExists);
        assert_eq!(names(&d), ["a", "b"]);
        assert!(same_item(&d.join("a"), &d.join("b")));
    }

    /// On every platform: refused with both names kept, or (Windows) done, the file under the
    /// new name. Never anything lost.
    #[test]
    fn rename_onto_another_hard_link_loses_nothing() {
        let d = tmp("alias-any");
        fs::write(d.join("a"), "mine").unwrap();
        fs::hard_link(d.join("a"), d.join("b")).unwrap();
        match rename(&d.join("a"), &d.join("b")) {
            Err(e) => {
                assert_eq!(e.kind(), ErrorKind::AlreadyExists);
                assert_eq!(names(&d), ["a", "b"]);
            }
            Ok(()) => assert_eq!(std::env::consts::OS, "windows", "only Windows renames onto its own hard link"),
        }
        assert_eq!(fs::read_to_string(d.join("b")).unwrap(), "mine");
    }

    /// Another app takes the new name between the two steps: the second step refuses, the item
    /// goes back to its old name, and the newcomer is untouched.
    #[cfg(unix)] // see `rename_onto_another_hard_link_is_refused`
    #[test]
    fn same_item_rename_never_replaces_what_takes_the_name_in_between() {
        let d = tmp("alias-race");
        fs::write(d.join("a"), "mine").unwrap();
        fs::hard_link(d.join("a"), d.join("b")).unwrap();
        let dd = d.clone();
        when_between(move || {
            fs::rename(dd.join("b"), dd.join("saved")).unwrap();
            fs::write(dd.join("b"), "OTHER APP DATA").unwrap();
        });
        assert_eq!(rename(&d.join("a"), &d.join("b")).unwrap_err().kind(), ErrorKind::AlreadyExists);
        assert_eq!(names(&d), ["a", "b", "saved"]);
        assert_eq!(fs::read_to_string(d.join("b")).unwrap(), "OTHER APP DATA");
        assert_eq!(fs::read_to_string(d.join("a")).unwrap(), "mine");
    }

    /// Both names taken between the steps: the item keeps its temporary name, and the error
    /// says what it is.
    #[cfg(unix)] // see `rename_onto_another_hard_link_is_refused`
    #[test]
    fn same_item_rename_says_where_it_left_the_item() {
        let d = tmp("alias-stuck");
        fs::write(d.join("a"), "mine").unwrap();
        fs::hard_link(d.join("a"), d.join("b")).unwrap();
        let dd = d.clone();
        when_between(move || {
            fs::remove_file(dd.join("b")).unwrap();
            fs::write(dd.join("b"), "OTHER b").unwrap();
            fs::write(dd.join("a"), "OTHER a").unwrap();
        });
        let e = rename(&d.join("a"), &d.join("b")).unwrap_err();
        let tmp = names(&d).into_iter().find(|n| n.starts_with(".arbor-")).expect("left under its temporary name");
        assert!(tmp.ends_with("-a"), "{tmp}");
        assert!(e.to_string().contains(&format!("it's now named “{tmp}” in the same folder")), "{e}");
        assert_eq!(fs::read_to_string(d.join(&tmp)).unwrap(), "mine");
        assert_eq!(fs::read_to_string(d.join("a")).unwrap(), "OTHER a");
        assert_eq!(fs::read_to_string(d.join("b")).unwrap(), "OTHER b");
    }

    #[test]
    fn copy_refuses_a_taken_name() {
        let d = tmp("copy");
        fs::write(d.join("a"), "new").unwrap();
        fs::write(d.join("b"), "old").unwrap();
        assert_eq!(copy_file(&d.join("a"), &d.join("b")).unwrap_err().kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(d.join("b")).unwrap(), "old");
        copy_file(&d.join("a"), &d.join("c")).unwrap();
        assert_eq!(fs::read_to_string(d.join("c")).unwrap(), "new");
    }

    #[test]
    fn identity_follows_renames_and_changes_on_replace() {
        let d = tmp("id");
        fs::write(d.join("a"), "a").unwrap();
        let id = |p: &Path| file_id(p, &p.symlink_metadata().unwrap()).unwrap();
        let first = id(&d.join("a"));
        rename(&d.join("a"), &d.join("b")).unwrap();
        assert_eq!(id(&d.join("b")), first);
        fs::write(d.join("c"), "a").unwrap();
        fs::remove_file(d.join("b")).unwrap();
        rename(&d.join("c"), &d.join("b")).unwrap();
        assert_ne!(id(&d.join("b")), first);
    }
}
