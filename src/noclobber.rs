//! Filesystem primitives that never replace an existing item (NORTHSTAR Safety #4). Every move,
//! rename and copy in Arbor goes through these, so a name taken between a check and the act
//! fails with `ErrorKind::AlreadyExists` instead of destroying what took it.
//!
//! - Moves and renames are atomic "rename unless the target exists": `renameat2` with
//!   `RENAME_NOREPLACE` on Linux, `renamex_np` with `RENAME_EXCL` on macOS, `MoveFileExW`
//!   without `MOVEFILE_REPLACE_EXISTING` on Windows. A disk that doesn't support the no-replace
//!   flag gets a fallback that still can't replace anything (see [`rename_fallback`]).
//! - File copies create the destination exclusively: `clonefile` on macOS (fails if it exists),
//!   `CopyFileW` with "fail if exists" on Windows, `O_CREAT | O_EXCL` elsewhere.
//!   Folders are made with `create_dir` and links with `symlink`, which fail if the name is taken.

use std::io::{self, ErrorKind};
use std::path::Path;

/// Moves `from` to `to` on the same disk, failing with `AlreadyExists` if anything is at `to`
/// (and with the platform's cross-device error across disks).
///
/// One exception: when `to` names `from` itself (a case-only rename on a case-insensitive
/// disk), it renames, since there's nothing else there to replace.
pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    match rename_excl(from, to) {
        Err(e) if e.kind() == ErrorKind::AlreadyExists && same_item(from, to) => std::fs::rename(from, to),
        #[cfg(unix)]
        Err(e) if unsupported(&e) => rename_fallback(from, to),
        r => r,
    }
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
/// - Folders: reserve `to` with `create_dir` (fails if it exists), then rename over that empty
///   folder, which `rename` only replaces while it's still empty. If something lands inside it
///   first, the rename fails and the folder is left.
#[cfg(unix)]
fn rename_fallback(from: &Path, to: &Path) -> io::Result<()> {
    let m = from.symlink_metadata()?;
    if m.is_dir() {
        std::fs::create_dir(to)?;
        return std::fs::rename(from, to).inspect_err(|_| drop(std::fs::remove_dir(to)));
    }
    match std::fs::hard_link(from, to) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists || is_cross_device(&e) => return Err(e),
        Err(e) => return Err(io::Error::new(e.kind(), format!("this disk can't move it without risking an overwrite ({e})"))),
    }
    // Both names are the same file now; taking `to` away again restores where it started.
    std::fs::remove_file(from).inspect_err(|_| drop(std::fs::remove_file(to)))
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
        drop(dst);
        let _ = std::fs::remove_file(to);
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
        assert_eq!(rename_fallback(&d.join("f"), &d.join("g")).unwrap_err().kind(), ErrorKind::AlreadyExists);
        assert_eq!(rename_fallback(&d.join("f"), &d.join("b")).unwrap_err().kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(d.join("b")).unwrap(), "b");
        rename_fallback(&d.join("a"), &d.join("c")).unwrap();
        rename_fallback(&d.join("f"), &d.join("h")).unwrap();
        assert!(!d.join("a").exists() && !d.join("f").exists());
        assert_eq!(fs::read_to_string(d.join("h/x")).unwrap(), "x");
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
