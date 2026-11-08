//! Moving files to the platform's trash. Returns where the item went when the platform says,
//! so a trash can be undone by moving it back.
//!
//! **A trash never silently becomes a permanent delete** (NORTHSTAR Safety #2). When an item's
//! disk has no trash that will take it, [`trash`] leaves it alone and says so
//! ([`TrashError::NoTrash`]); the UI then asks, in Arbor's one dialog, whether to delete it
//! permanently (`confirm.rs`).
//!
//! - **macOS:** `NSFileManager.trashItemAtURL`, which fails rather than deleting on a volume
//!   with no trash; that failure becomes [`TrashError::NoTrash`] ([`classify_mac`]).
//! - **Linux:** the freedesktop.org trash spec: the home trash for items on its disk, else the
//!   disk's own `$topdir/.Trash/$uid` (only if `.Trash` is a sticky folder, not a link) or
//!   `$topdir/.Trash-$uid` (made if missing). An item is only ever renamed into a trash on its
//!   own disk, never copied across.
//! - **Windows:** the Recycle Bin through `IFileOperation`, refusing each item before the shell
//!   touches it unless the shell is about to recycle it (see [`recycle`]); a drive that isn't a
//!   fixed local disk is refused before the shell is asked at all ([`precheck`]).

use std::path::{Path, PathBuf};

/// What the platform calls its trash.
pub const BIN: &str = if cfg!(windows) { "Recycle Bin" } else { "Trash" };

/// Why an item didn't go to the trash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrashError {
    /// Its disk has no trash that will take it. The item is untouched; only a permanent
    /// delete, confirmed, would remove it. The words finish "can't go to the Trash: …".
    NoTrash(String),
    /// Anything else. The item is untouched.
    Failed(String),
    /// Windows deleted it for good though it said it would recycle it (the item didn't land in
    /// the Recycle Bin). Never expected; reported plainly if it happens.
    #[cfg_attr(not(windows), allow(dead_code))]
    DeletedForGood,
}

impl std::fmt::Display for TrashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrashError::NoTrash(why) => write!(f, "can't go to the {BIN}: {why}"),
            TrashError::Failed(why) => f.write_str(why),
            TrashError::DeletedForGood => write!(f, "Windows deleted it permanently instead of putting it in the {BIN}"),
        }
    }
}

// ---- macOS ------------------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub fn trash(path: &Path) -> Result<Option<PathBuf>, TrashError> {
    use objc2_foundation::{NSFileManager, NSURL};
    let url = NSURL::from_file_path(path).ok_or_else(|| TrashError::Failed("bad path".into()))?;
    let fm = NSFileManager::defaultManager();
    let mut out = None;
    match fm.trashItemAtURL_resultingItemURL_error(&url, Some(&mut out)) {
        Ok(()) => Ok(out.and_then(|u| u.to_file_path())),
        Err(e) => {
            let deletable = path.parent().is_some_and(writable);
            let reason = e.localizedFailureReason().map(|r| r.to_string());
            Err(classify_mac(&e.domain().to_string(), e.code() as i64, deletable, e.localizedDescription().to_string(), reason))
        }
    }
}

/// What a failed `trashItemAtURL` means. `NSFeatureUnsupportedError` (3328) is its answer on a
/// volume with no trash (many SMB and NFS shares, some FAT, exFAT and NTFS disks). A write
/// error (512, 513) often comes from a volume whose `.Trashes` can't be made, but it's a generic
/// write or permission failure, not proof the disk has no Trash, so the words keep macOS's own
/// reason. Either way the item stays and the permanent-delete question is offered, but only when
/// the item's own folder can be changed: otherwise a permanent delete couldn't remove it either,
/// and offering one would only fail.
#[cfg(any(target_os = "macos", test))]
pub fn classify_mac(domain: &str, code: i64, folder_writable: bool, message: String, reason: Option<String>) -> TrashError {
    const FEATURE_UNSUPPORTED: i64 = 3328;
    const WRITE_UNKNOWN: i64 = 512;
    const WRITE_NO_PERMISSION: i64 = 513;
    if domain != "NSCocoaErrorDomain" || !folder_writable {
        return TrashError::Failed(message);
    }
    match code {
        FEATURE_UNSUPPORTED => TrashError::NoTrash("the disk it's on has no Trash".into()),
        WRITE_UNKNOWN | WRITE_NO_PERMISSION => {
            let why = reason.filter(|r| !r.trim().is_empty()).unwrap_or(message);
            TrashError::NoTrash(format!("macOS couldn't move it there ({})", why.trim().trim_end_matches('.')))
        }
        _ => TrashError::Failed(message),
    }
}

/// Whether this process may add and remove entries in folder `dir` (a read-only disk isn't).
#[cfg(unix)]
fn writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return false };
    // SAFETY: `c` is a valid NUL-terminated path.
    unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

// ---- Linux and other freedesktop systems --------------------------------------------------------

/// The freedesktop.org trash spec (see the module docs).
#[cfg(all(unix, not(target_os = "macos")))]
pub fn trash(path: &Path) -> Result<Option<PathBuf>, TrashError> {
    use std::os::unix::fs::MetadataExt;
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| crate::fs::home().join(".local/share"));
    let home = data.join("Trash");
    let dev = path.symlink_metadata().map_err(|e| TrashError::Failed(e.to_string()))?.dev();
    // The home trash is for items on its own disk (the nearest folder of it that exists says
    // which, before it's made).
    let home_dev = home.ancestors().find_map(|a| a.metadata().ok()).map(|m| m.dev());
    let home = (home_dev == Some(dev)).then_some(home.as_path());
    let dir = path.parent().ok_or_else(|| TrashError::Failed("no folder".into()))?;
    let top = mount_top(dir, dev);
    // SAFETY: getuid can't fail.
    trash_with(path, home, &top, unsafe { libc::getuid() })
}

/// The top folder of the disk holding `dir` (whose device is `dev`): the last folder up from it
/// on the same device, links in its path resolved first.
#[cfg(all(unix, not(target_os = "macos")))]
fn mount_top(dir: &Path, dev: u64) -> PathBuf {
    use std::os::unix::fs::MetadataExt;
    let mut top = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    while let Some(up) = top.parent() {
        if up.metadata().ok().map(|m| m.dev()) != Some(dev) {
            break;
        }
        top = up.to_path_buf();
    }
    top
}

/// [`trash`] with its choices made: `home` is the home trash when the item is on its disk, and
/// `top` the top folder of the item's disk. Tries the home trash, then `top/.Trash/uid`, then
/// `top/.Trash-uid`; the first that takes the item wins. When none can, the item stays where it
/// is: [`TrashError::NoTrash`] if a permanent delete could still remove it (its folder can be
/// changed), else [`TrashError::Failed`].
#[cfg(all(unix, not(target_os = "macos")))]
fn trash_with(path: &Path, home: Option<&Path>, top: &Path, uid: u32) -> Result<Option<PathBuf>, TrashError> {
    let abs = std::path::absolute(path).map_err(|e| TrashError::Failed(e.to_string()))?;
    let name = abs.file_name().ok_or_else(|| TrashError::Failed("no file name".into()))?;
    let dir = abs.parent().ok_or_else(|| TrashError::Failed("no folder".into()))?;
    let mut why: Option<String> = None;
    if let Some(home) = home {
        match trash_in(home, path, &abs) {
            Ok(at) => return Ok(Some(at)),
            Err(Stage::Setup(e)) => why = Some(e),
            Err(Stage::Item(e)) => return Err(TrashError::Failed(e.to_string())),
        }
    }
    // In a disk's own trash, the item's path is written relative to the disk's top folder.
    let canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()).join(name);
    let rel = canon.strip_prefix(top).map(Path::to_path_buf).unwrap_or(canon.clone());
    for root in [admin_trash(top, uid), user_trash(top, uid)] {
        let root = match root {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(e) => {
                why = Some(e);
                continue;
            }
        };
        match trash_in(&root, path, &rel) {
            Ok(at) => return Ok(Some(at)),
            Err(Stage::Setup(e)) => why = Some(e),
            Err(Stage::Item(e)) => return Err(TrashError::Failed(e.to_string())),
        }
    }
    if !writable(dir) {
        return Err(TrashError::Failed(format!("“{}” can't be changed (read-only, or no permission)", dir.display())));
    }
    let why = why.map_or_else(String::new, |w| format!(" ({w})"));
    Err(TrashError::NoTrash(format!("“{}” has no trash Arbor can use{why}", top.display())))
}

/// How a try at one trash folder failed.
#[cfg(all(unix, not(target_os = "macos")))]
enum Stage {
    /// The trash folder couldn't be used (made, written to, or it's on another disk): the
    /// next one may do. The item is untouched.
    Setup(String),
    /// Moving the item itself failed for a reason no other trash would change. Untouched.
    Item(std::io::Error),
}

/// `top/.Trash/uid`, made if missing, when `top/.Trash` is a sticky folder (not a link), as
/// the spec requires; `None` when there's no `.Trash`.
#[cfg(all(unix, not(target_os = "macos")))]
fn admin_trash(top: &Path, uid: u32) -> Result<Option<PathBuf>, String> {
    use std::os::unix::fs::PermissionsExt;
    let shared = top.join(".Trash");
    let Ok(m) = shared.symlink_metadata() else { return Ok(None) };
    if m.file_type().is_symlink() || !m.is_dir() {
        return Err(format!("{} isn't a folder", shared.display()));
    }
    if m.permissions().mode() & 0o1000 == 0 {
        return Err(format!("{} isn't sticky, so it isn't safe to share", shared.display()));
    }
    own_folder(&shared.join(uid.to_string()), uid).map(Some)
}

/// `top/.Trash-uid`, made if missing.
#[cfg(all(unix, not(target_os = "macos")))]
fn user_trash(top: &Path, uid: u32) -> Result<Option<PathBuf>, String> {
    own_folder(&top.join(format!(".Trash-{uid}")), uid).map(Some)
}

/// Folder `p`, made (owner only) if missing; it must be a real folder, not a link, and ours.
#[cfg(all(unix, not(target_os = "macos")))]
fn own_folder(p: &Path, uid: u32) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::DirBuilder::new().mode(0o700).create(p) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("couldn't make {}: {e}", p.display())),
    }
    let m = p.symlink_metadata().map_err(|e| format!("{}: {e}", p.display()))?;
    if m.file_type().is_symlink() || !m.is_dir() {
        return Err(format!("{} isn't a folder", p.display()));
    }
    if m.uid() != uid {
        return Err(format!("{} belongs to someone else", p.display()));
    }
    Ok(p.to_path_buf())
}

/// Moves `path` into trash folder `root` (`root/files`, with its `.trashinfo` in `root/info`
/// saying it came from `orig`).
#[cfg(all(unix, not(target_os = "macos")))]
fn trash_in(root: &Path, path: &Path, orig: &Path) -> Result<PathBuf, Stage> {
    use std::io::Write;
    let setup = |e: std::io::Error| Stage::Setup(format!("{}: {e}", root.display()));
    let (files, info) = (root.join("files"), root.join("info"));
    std::fs::create_dir_all(&files).map_err(setup)?;
    std::fs::create_dir_all(&info).map_err(setup)?;
    let name = path.file_name().ok_or_else(|| Stage::Setup("no file name".into()))?.to_string_lossy().into_owned();
    let body = format!("[Trash Info]\nPath={}\nDeletionDate={}\n", url_escape(orig), deletion_date());
    // Reserve a unique name by creating its .trashinfo exclusively, then move the item in
    // without replacing anything (a stray file under that name in `files` gets the next name).
    for n in 1..10_000 {
        let stem = if n == 1 { name.clone() } else { format!("{name}.{n}") };
        let info_path = info.join(format!("{stem}.trashinfo"));
        let mut f = match std::fs::OpenOptions::new().write(true).create_new(true).open(&info_path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(setup(e)),
        };
        if let Err(e) = f.write_all(body.as_bytes()) {
            let _ = std::fs::remove_file(&info_path);
            return Err(setup(e));
        }
        let dest = files.join(&stem);
        match crate::noclobber::rename(path, &dest) {
            Ok(()) => return Ok(dest),
            Err(e) => {
                let _ = std::fs::remove_file(&info_path);
                match e.kind() {
                    std::io::ErrorKind::AlreadyExists => {}
                    // On another disk after all (a bind mount): another trash may be on this one.
                    std::io::ErrorKind::CrossesDevices => return Err(setup(e)),
                    _ => return Err(Stage::Item(e)),
                }
            }
        }
    }
    Err(Stage::Setup(format!("{}: no free name", root.display())))
}

/// After an item at `trashed` (`Trash/files/NAME`) was put back: removes its
/// `Trash/info/NAME.trashinfo`, which would otherwise list it in the trash still.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn forget(trashed: &Path) {
    let (Some(files), Some(name)) = (trashed.parent(), trashed.file_name()) else { return };
    if files.file_name() != Some("files".as_ref()) {
        return;
    }
    let Some(root) = files.parent() else { return };
    let mut info = name.to_os_string();
    info.push(".trashinfo");
    let _ = std::fs::remove_file(root.join("info").join(info));
}

/// macOS keeps no record beside the item, and Windows trash can't be undone from Arbor.
#[cfg(not(all(unix, not(target_os = "macos"))))]
pub fn forget(_trashed: &Path) {}

#[cfg(all(unix, not(target_os = "macos")))]
fn url_escape(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut s = String::new();
    for &b in p.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

#[cfg(all(unix, not(target_os = "macos")))]
fn deletion_date() -> String {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    let t = rune::LocalTime::at(ms);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second)
}

// ---- Windows ----------------------------------------------------------------------------------

/// `GetDriveTypeW`'s answers.
#[cfg(any(windows, test))]
mod drive {
    pub const REMOVABLE: u32 = 2;
    pub const FIXED: u32 = 3;
    pub const REMOTE: u32 = 4;
    pub const CDROM: u32 = 5;
    pub const RAMDISK: u32 = 6;
}

/// Before asking the shell: only a fixed local disk can have a Recycle Bin. Windows keeps none
/// on network shares (mapped drives and `\\server\share` paths), removable drives (USB sticks,
/// SD cards), optical discs and RAM disks, and deletes from them for good. `root` is the
/// volume's root as `GetVolumePathNameW` gives it (`E:\`, `\\server\share\`), `kind` its
/// `GetDriveTypeW`. Returns why it can't be recycled, or `None` to go on to the shell (which
/// still refuses per item, see [`recycle`]).
#[cfg(any(windows, test))]
pub fn precheck(root: &str, kind: u32) -> Option<String> {
    let name = root.trim_end_matches('\\');
    let place = if name.len() == 2 && name.ends_with(':') { format!("drive {name}") } else { format!("“{name}”") };
    match kind {
        drive::FIXED if !root.starts_with(r"\\") || root.starts_with(r"\\?\") && !root.starts_with(r"\\?\UNC\") => None,
        drive::FIXED | drive::REMOTE => Some(format!("{place} is a network location, which has no {BIN}")),
        drive::REMOVABLE => Some(format!("{place} is a removable drive, and Windows keeps no {BIN} on those")),
        drive::CDROM => Some(format!("{place} is a disc, which has no {BIN}")),
        drive::RAMDISK => Some(format!("{place} is a RAM disk, which has no {BIN}")),
        _ => Some(format!("Arbor can't tell whether {place} has a {BIN}")),
    }
}

/// What the shell's per-item answers add up to (see [`recycle`]): `refused` when Arbor stopped
/// the delete before it began because the shell wasn't going to recycle the item; `performed`
/// what `PerformOperations` returned; `deleted` what `PostDeleteItem` reported (its HRESULT,
/// and whether the item landed in the Recycle Bin), `None` if it never came.
#[cfg(any(windows, test))]
pub fn verdict(refused: bool, performed: i32, deleted: Option<(i32, bool)>, place: &str) -> Result<(), TrashError> {
    if refused {
        return Err(TrashError::NoTrash(format!(
            "Windows won't put it in the {BIN} of {place} (it may be too big for it, or the {BIN} is turned off there)"
        )));
    }
    match deleted {
        Some((hr, true)) if hr >= 0 && performed >= 0 => Ok(()),
        Some((hr, false)) if hr >= 0 => Err(TrashError::DeletedForGood),
        Some((hr, _)) if hr < 0 => Err(TrashError::Failed(format!("the {BIN} refused it (code {hr:#010x})"))),
        _ if performed < 0 => Err(TrashError::Failed(format!("the {BIN} refused it (code {performed:#010x})"))),
        _ => Err(TrashError::Failed(format!("Windows didn't say whether it went to the {BIN}"))),
    }
}

/// The Recycle Bin. Windows doesn't say where the item went (Arbor can't undo it; restore it
/// from the Recycle Bin), so this returns `None` on success.
#[cfg(windows)]
pub fn trash(path: &Path) -> Result<Option<PathBuf>, TrashError> {
    let abs = std::path::absolute(path).map_err(|e| TrashError::Failed(e.to_string()))?;
    let (root, kind) = win::volume(&abs);
    if let Some(why) = precheck(&root, kind) {
        return Err(TrashError::NoTrash(why));
    }
    recycle(&abs, &root)
}

/// The shell's Recycle Bin, item by item, through `IFileOperation` with
/// `FOFX_RECYCLEONDELETE` and no UI. The shell calls our progress sink's `PreDeleteItem` before
/// it deletes, with `TSF_DELETE_RECYCLE_IF_POSSIBLE` set only when it's about to recycle the
/// item; without it (no Recycle Bin on the volume, the item too big for it, the bin turned off
/// for the drive, a virtual drive that doesn't support it), the sink returns `E_ABORT`, which
/// cancels that delete before anything is removed. Electron's `shell.trashItem` relies on the
/// same contract (`platform_util_win.cc`). `PostDeleteItem` then says whether the item landed
/// in the bin (its new item is null when it was deleted for good).
///
/// `SHFileOperationW` with `FOF_WANTNUKEWARNING` isn't enough: with `FOF_NOCONFIRMATION`
/// it deletes from some drives (WinFsp and Cryptomator volumes, for one) for good without the
/// warning, and its warning is Windows' own dialog, not Arbor's.
#[cfg(windows)]
fn recycle(abs: &Path, root: &str) -> Result<Option<PathBuf>, TrashError> {
    let r = win::recycle(abs)?;
    let place = root.trim_end_matches('\\');
    let place = if place.len() == 2 { format!("drive {place}") } else { format!("“{place}”") };
    verdict(r.refused, r.performed, r.deleted, &place).map(|()| None)
}

#[cfg(windows)]
mod win {
    //! The COM plumbing for [`super::recycle`]: `IFileOperation` and a progress sink, by hand
    //! (windows-sys has no COM interfaces). Vtable layouts follow `shobjidl_core.h`, checked
    //! against the `windows` crate's `IFileOperation_Vtbl` and `IFileOperationProgressSink_Vtbl`.

    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering::SeqCst};

    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumePathNameW};
    use windows_sys::Win32::System::Com::{
        CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };
    use windows_sys::Win32::UI::Shell::{
        FOF_ALLOWUNDO, FOF_NO_UI, FOFX_RECYCLEONDELETE, FileOperation, SHCreateItemFromParsingName, TSF_DELETE_RECYCLE_IF_POSSIBLE,
    };
    use windows_sys::core::{GUID, HRESULT};

    use super::TrashError;

    const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
    const IID_IFILEOPERATION: GUID = GUID::from_u128(0x947aab5f_0a5c_4c13_b4d6_4bf7836fc9f8);
    const IID_ISINK: GUID = GUID::from_u128(0x04b0f1a7_9490_44bc_96e1_4296a31252e2);
    const IID_ISHELLITEM: GUID = GUID::from_u128(0x43826d1e_e718_42ee_bc55_a1e261c37bfe);
    const S_OK: HRESULT = 0;
    const E_NOINTERFACE: HRESULT = 0x80004002_u32 as i32;
    const E_ABORT: HRESULT = 0x80004004_u32 as i32;

    fn wide(p: &std::ffi::OsStr) -> Vec<u16> {
        p.encode_wide().chain([0]).collect()
    }

    /// The volume `abs` is on: its root (`C:\`, `\\server\share\`) and drive type.
    pub fn volume(abs: &Path) -> (String, u32) {
        let path = wide(abs.as_os_str());
        let mut buf = [0u16; 1024];
        // SAFETY: `path` is NUL-terminated and `buf` is as long as we say.
        let ok = unsafe { GetVolumePathNameW(path.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } != 0;
        if !ok {
            return (abs.display().to_string(), 0);
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        // SAFETY: `buf` holds a NUL-terminated root path.
        let kind = unsafe { GetDriveTypeW(buf.as_ptr()) };
        (String::from_utf16_lossy(&buf[..n]), kind)
    }

    /// What the shell said about one delete.
    pub struct Recycled {
        pub refused: bool,
        pub performed: HRESULT,
        pub deleted: Option<(HRESULT, bool)>,
    }

    // Read by COM through pointers, not by name.
    #[allow(dead_code)]
    #[repr(C)]
    struct UnknownVtbl {
        query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
    }

    #[allow(dead_code)]
    #[repr(C)]
    struct FileOpVtbl {
        base: UnknownVtbl,
        advise: usize,
        unadvise: usize,
        set_operation_flags: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
        set_progress_message: usize,
        set_progress_dialog: usize,
        set_properties: usize,
        set_owner_window: usize,
        apply_properties_to_item: usize,
        apply_properties_to_items: usize,
        rename_item: usize,
        rename_items: usize,
        move_item: usize,
        move_items: usize,
        copy_item: usize,
        copy_items: usize,
        delete_item: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> HRESULT,
        delete_items: usize,
        new_item: usize,
        perform_operations: unsafe extern "system" fn(*mut c_void) -> HRESULT,
        get_any_operations_aborted: usize,
    }

    type P = *mut c_void;
    type W = *const u16;

    #[allow(dead_code)]
    #[repr(C)]
    struct SinkVtbl {
        base: UnknownVtbl,
        start_operations: unsafe extern "system" fn(P) -> HRESULT,
        finish_operations: unsafe extern "system" fn(P, HRESULT) -> HRESULT,
        pre_rename_item: unsafe extern "system" fn(P, u32, P, W) -> HRESULT,
        post_rename_item: unsafe extern "system" fn(P, u32, P, W, HRESULT, P) -> HRESULT,
        pre_move_item: unsafe extern "system" fn(P, u32, P, P, W) -> HRESULT,
        post_move_item: unsafe extern "system" fn(P, u32, P, P, W, HRESULT, P) -> HRESULT,
        pre_copy_item: unsafe extern "system" fn(P, u32, P, P, W) -> HRESULT,
        post_copy_item: unsafe extern "system" fn(P, u32, P, P, W, HRESULT, P) -> HRESULT,
        pre_delete_item: unsafe extern "system" fn(P, u32, P) -> HRESULT,
        post_delete_item: unsafe extern "system" fn(P, u32, P, HRESULT, P) -> HRESULT,
        pre_new_item: unsafe extern "system" fn(P, u32, P, W) -> HRESULT,
        post_new_item: unsafe extern "system" fn(P, u32, P, W, W, u32, HRESULT, P) -> HRESULT,
        update_progress: unsafe extern "system" fn(P, u32, u32) -> HRESULT,
        reset_timer: unsafe extern "system" fn(P) -> HRESULT,
        pause_timer: unsafe extern "system" fn(P) -> HRESULT,
        resume_timer: unsafe extern "system" fn(P) -> HRESULT,
    }

    /// Our `IFileOperationProgressSink`: a heap object whose first field is its vtable, freed
    /// when its last reference goes.
    #[allow(dead_code)]
    #[repr(C)]
    struct Sink {
        vtbl: &'static SinkVtbl,
        refs: AtomicU32,
        refused: AtomicBool,
        /// PostDeleteItem came: its HRESULT, and whether the item landed in the bin.
        posted: AtomicBool,
        hr: AtomicI32,
        landed: AtomicBool,
    }

    unsafe extern "system" fn qi(this: P, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
        if out.is_null() {
            return E_NOINTERFACE;
        }
        // SAFETY: COM passes a valid IID pointer and out-pointer.
        let id = unsafe { &*riid };
        let eq = |g: &GUID| g.data1 == id.data1 && g.data2 == id.data2 && g.data3 == id.data3 && g.data4 == id.data4;
        if eq(&IID_IUNKNOWN) || eq(&IID_ISINK) {
            // SAFETY: as above; `this` is a live Sink.
            unsafe {
                *out = this;
                add_ref(this);
            }
            S_OK
        } else {
            // SAFETY: as above.
            unsafe { *out = std::ptr::null_mut() };
            E_NOINTERFACE
        }
    }
    unsafe extern "system" fn add_ref(this: P) -> u32 {
        // SAFETY: `this` is a live Sink.
        unsafe { &*(this as *const Sink) }.refs.fetch_add(1, SeqCst) + 1
    }
    unsafe extern "system" fn release(this: P) -> u32 {
        // SAFETY: `this` is a live Sink; the last reference frees the Box it came from.
        let left = unsafe { &*(this as *const Sink) }.refs.fetch_sub(1, SeqCst) - 1;
        if left == 0 {
            drop(unsafe { Box::from_raw(this as *mut Sink) });
        }
        left
    }
    unsafe extern "system" fn ok0(_: P) -> HRESULT {
        S_OK
    }
    unsafe extern "system" fn finish(_: P, _: HRESULT) -> HRESULT {
        S_OK
    }
    unsafe extern "system" fn pre_rename(_: P, _: u32, _: P, _: W) -> HRESULT {
        S_OK
    }
    unsafe extern "system" fn post_rename(_: P, _: u32, _: P, _: W, _: HRESULT, _: P) -> HRESULT {
        S_OK
    }
    unsafe extern "system" fn pre_move(_: P, _: u32, _: P, _: P, _: W) -> HRESULT {
        S_OK
    }
    unsafe extern "system" fn post_move(_: P, _: u32, _: P, _: P, _: W, _: HRESULT, _: P) -> HRESULT {
        S_OK
    }
    /// Refuses, before anything is removed, any delete the shell isn't going to recycle.
    unsafe extern "system" fn pre_delete(this: P, flags: u32, _: P) -> HRESULT {
        if flags & TSF_DELETE_RECYCLE_IF_POSSIBLE as u32 == 0 {
            // SAFETY: `this` is a live Sink.
            unsafe { &*(this as *const Sink) }.refused.store(true, SeqCst);
            return E_ABORT;
        }
        S_OK
    }
    unsafe extern "system" fn post_delete(this: P, _: u32, _: P, hr: HRESULT, new_item: P) -> HRESULT {
        // SAFETY: `this` is a live Sink.
        let s = unsafe { &*(this as *const Sink) };
        s.hr.store(hr, SeqCst);
        s.landed.store(!new_item.is_null(), SeqCst);
        s.posted.store(true, SeqCst);
        S_OK
    }
    unsafe extern "system" fn pre_new(_: P, _: u32, _: P, _: W) -> HRESULT {
        S_OK
    }
    unsafe extern "system" fn post_new(_: P, _: u32, _: P, _: W, _: W, _: u32, _: HRESULT, _: P) -> HRESULT {
        S_OK
    }
    unsafe extern "system" fn progress(_: P, _: u32, _: u32) -> HRESULT {
        S_OK
    }

    static SINK_VTBL: SinkVtbl = SinkVtbl {
        base: UnknownVtbl { query_interface: qi, add_ref, release },
        start_operations: ok0,
        finish_operations: finish,
        pre_rename_item: pre_rename,
        post_rename_item: post_rename,
        pre_move_item: pre_move,
        post_move_item: post_move,
        pre_copy_item: pre_move,
        post_copy_item: post_move,
        pre_delete_item: pre_delete,
        post_delete_item: post_delete,
        pre_new_item: pre_new,
        post_new_item: post_new,
        update_progress: progress,
        reset_timer: ok0,
        pause_timer: ok0,
        resume_timer: ok0,
    };

    /// Releases a COM object.
    unsafe fn release_obj(p: P) {
        if !p.is_null() {
            // SAFETY: `p` is a live COM object, whose first field points at an IUnknown vtable.
            unsafe { ((**(p as *mut *const UnknownVtbl)).release)(p) };
        }
    }

    fn fail(what: &str, hr: HRESULT) -> TrashError {
        TrashError::Failed(format!("{what} (code {hr:#010x})"))
    }

    /// One `IFileOperation` deleting `abs` to the Recycle Bin, with our sink watching. The
    /// worker thread is its own single-threaded apartment for the call.
    pub fn recycle(abs: &Path) -> Result<Recycled, TrashError> {
        // SAFETY: CoUninitialize balances a successful CoInitializeEx on this thread.
        unsafe {
            let init = CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32);
            if init < 0 {
                return Err(fail("couldn't start the shell's file operations", init));
            }
            let r = file_operation(abs);
            CoUninitialize();
            r
        }
    }

    /// SAFETY: COM is initialized on this thread as a single-threaded apartment.
    unsafe fn file_operation(abs: &Path) -> Result<Recycled, TrashError> {
        let mut op: P = std::ptr::null_mut();
        // SAFETY: valid GUIDs and out-pointer.
        let hr = unsafe { CoCreateInstance(&FileOperation, std::ptr::null_mut(), CLSCTX_ALL, &IID_IFILEOPERATION, &mut op) };
        if hr < 0 || op.is_null() {
            return Err(fail("couldn't start the shell's file operations", hr));
        }
        let sink = Box::into_raw(Box::new(Sink {
            vtbl: &SINK_VTBL,
            refs: AtomicU32::new(1),
            refused: AtomicBool::new(false),
            posted: AtomicBool::new(false),
            hr: AtomicI32::new(0),
            landed: AtomicBool::new(false),
        })) as P;
        let mut item: P = std::ptr::null_mut();
        // SAFETY: `op` is a live IFileOperation and `sink` a live Sink; each object is released
        // once, here, after the operation is done with it.
        unsafe {
            let r = delete_one(op, abs, sink, &mut item);
            release_obj(item);
            release_obj(op);
            release_obj(sink);
            r
        }
    }

    /// SAFETY: `op` is a live IFileOperation and `sink` a live Sink. Leaves the shell item it
    /// makes in `item` for the caller to release.
    unsafe fn delete_one(op: P, abs: &Path, sink: P, item: &mut P) -> Result<Recycled, TrashError> {
        // SAFETY: as the function's contract says; `path` is NUL-terminated and outlives the calls.
        unsafe {
            let vt = &**(op as *mut *const FileOpVtbl);
            let hr = (vt.set_operation_flags)(op, FOF_NO_UI | FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE);
            if hr < 0 {
                return Err(fail("the shell refused the Recycle Bin flags", hr));
            }
            let path = wide(abs.as_os_str());
            let hr = SHCreateItemFromParsingName(path.as_ptr(), std::ptr::null_mut(), &IID_ISHELLITEM, item);
            if hr < 0 || item.is_null() {
                return Err(fail("the shell couldn't find it", hr));
            }
            let hr = (vt.delete_item)(op, *item, sink);
            if hr < 0 {
                return Err(fail("the shell couldn't queue it", hr));
            }
            let performed = (vt.perform_operations)(op);
            let s = &*(sink as *const Sink);
            let deleted = s.posted.load(SeqCst).then(|| (s.hr.load(SeqCst), s.landed.load(SeqCst)));
            Ok(Recycled { refused: s.refused.load(SeqCst), performed, deleted })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(unix, not(target_os = "macos")))]
    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-trash-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn uid() -> u32 {
        // SAFETY: getuid can't fail.
        unsafe { libc::getuid() }
    }

    /// Permission tricks don't stop root.
    #[cfg(all(unix, not(target_os = "macos")))]
    fn root() -> bool {
        // SAFETY: geteuid can't fail.
        unsafe { libc::geteuid() == 0 }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn chmod(p: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn freedesktop_trash_and_restore_leave_no_record() {
        let d = tmp("restore");
        let f = d.join("a b.txt");
        std::fs::write(&f, "x").unwrap();
        let at = super::trash_in(&d.join("Trash"), &f, &f).ok().unwrap();
        assert_eq!(at, d.join("Trash/files/a b.txt"));
        let info = std::fs::read_to_string(d.join("Trash/info/a b.txt.trashinfo")).unwrap();
        assert!(info.contains("a%20b.txt"), "{info}");
        std::fs::rename(&at, &f).unwrap();
        super::forget(&at);
        assert!(!d.join("Trash/info/a b.txt.trashinfo").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A file in `Trash/files` with no `.trashinfo` (left by another app) isn't replaced.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn freedesktop_trash_never_overwrites_a_stray_file() {
        let d = tmp("stray");
        std::fs::create_dir_all(d.join("Trash/files")).unwrap();
        std::fs::write(d.join("Trash/files/a"), "stray").unwrap();
        std::fs::write(d.join("a"), "mine").unwrap();
        let at = super::trash_in(&d.join("Trash"), &d.join("a"), &d.join("a")).ok().unwrap();
        assert_eq!(at, d.join("Trash/files/a.2"));
        assert_eq!(std::fs::read_to_string(d.join("Trash/files/a")).unwrap(), "stray");
        assert_eq!(std::fs::read_to_string(&at).unwrap(), "mine");
        assert!(!d.join("Trash/info/a.trashinfo").exists() && d.join("Trash/info/a.2.trashinfo").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The home trash takes items on its disk.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_home_trash_comes_first_on_its_disk() {
        let d = tmp("home");
        std::fs::write(d.join("a"), "x").unwrap();
        let at = trash_with(&d.join("a"), Some(&d.join("home/Trash")), &d, uid()).unwrap().unwrap();
        assert_eq!(at, d.join("home/Trash/files/a"));
        assert!(!d.join(format!(".Trash-{}", uid())).exists(), "no disk trash made");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// On another disk: `$topdir/.Trash/$uid` when `.Trash` is sticky, with the path written
    /// relative to the disk's top folder.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_disk_with_a_sticky_shared_trash_uses_it() {
        let d = tmp("sticky");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::create_dir(d.join(".Trash")).unwrap();
        chmod(&d.join(".Trash"), 0o1777);
        std::fs::write(d.join("sub/a"), "x").unwrap();
        let top = std::fs::canonicalize(&d).unwrap();
        let at = trash_with(&d.join("sub/a"), None, &top, uid()).unwrap().unwrap();
        assert_eq!(at, top.join(format!(".Trash/{}/files/a", uid())));
        let info = std::fs::read_to_string(top.join(format!(".Trash/{}/info/a.trashinfo", uid()))).unwrap();
        assert!(info.contains("\nPath=sub/a\n"), "{info}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A shared `.Trash` that isn't sticky, or is a link, is never used: `.Trash-$uid` is.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn an_unsafe_shared_trash_is_skipped_for_the_users_own() {
        for link in [false, true] {
            let d = tmp(if link { "link" } else { "unsticky" });
            std::fs::write(d.join("a"), "x").unwrap();
            if link {
                std::fs::create_dir(d.join("elsewhere")).unwrap();
                chmod(&d.join("elsewhere"), 0o1777);
                std::os::unix::fs::symlink(d.join("elsewhere"), d.join(".Trash")).unwrap();
            } else {
                std::fs::create_dir(d.join(".Trash")).unwrap();
                chmod(&d.join(".Trash"), 0o777);
            }
            let at = trash_with(&d.join("a"), None, &d, uid()).unwrap().unwrap();
            assert_eq!(at, d.join(format!(".Trash-{}/files/a", uid())), "link {link}");
            assert!(std::fs::read_dir(d.join(".Trash")).unwrap().next().is_none(), "nothing put in the shared one");
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// A disk where no trash can be made (its top folder can't be written, as on a disk mounted
    /// read-only at the top, or owned by someone else) leaves the item and offers a permanent
    /// delete, as long as the item's own folder can be changed; otherwise it's an error, since
    /// a delete couldn't remove it either. A `.Trash-$uid` that's a link is never followed.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn no_usable_trash_offers_a_permanent_delete_and_never_deletes() {
        if root() {
            eprintln!("skipped: permissions don't stop root");
            return;
        }
        let d = tmp("none");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/a"), "x").unwrap();
        chmod(&d, 0o555);
        let r = trash_with(&d.join("sub/a"), None, &d, uid());
        assert!(matches!(&r, Err(TrashError::NoTrash(w)) if w.contains("has no trash Arbor can use")), "{r:?}");
        assert!(d.join("sub/a").exists(), "left where it was");
        // Its folder can't be changed either: an error, no offer.
        chmod(&d.join("sub"), 0o555);
        let r = trash_with(&d.join("sub/a"), None, &d, uid());
        assert!(matches!(&r, Err(TrashError::Failed(w)) if w.contains("can't be changed")), "{r:?}");
        chmod(&d.join("sub"), 0o755);
        chmod(&d, 0o755);
        // A link where .Trash-$uid goes: refused, not followed.
        std::fs::create_dir(d.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(d.join("elsewhere"), d.join(format!(".Trash-{}", uid()))).unwrap();
        let r = trash_with(&d.join("sub/a"), None, &d, uid());
        assert!(matches!(&r, Err(TrashError::NoTrash(w)) if w.contains("isn't a folder")), "{r:?}");
        assert!(d.join("sub/a").exists() && std::fs::read_dir(d.join("elsewhere")).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A real second disk (tmpfs at /dev/shm, when the temp folder isn't on it): its top is
    /// found by device, and the home trash isn't offered for it.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_top_of_another_disk_is_found_by_device() {
        use std::os::unix::fs::MetadataExt;
        let shm = Path::new("/dev/shm");
        let (Ok(s), Ok(t)) = (shm.metadata(), std::env::temp_dir().metadata()) else { return };
        if s.dev() == t.dev() {
            eprintln!("skipped: /dev/shm is on the temp folder's disk");
            return;
        }
        let d = shm.join(format!("arbor-top-{}", std::process::id()));
        std::fs::create_dir_all(d.join("x/y")).unwrap();
        assert_eq!(mount_top(&d.join("x/y"), s.dev()), shm);
        assert_eq!(mount_top(&std::env::temp_dir(), t.dev()).metadata().unwrap().dev(), t.dev());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn mac_failures_offer_a_permanent_delete_only_for_a_disk_with_no_trash() {
        let msg = || "nope".to_string();
        assert_eq!(
            classify_mac("NSCocoaErrorDomain", 3328, true, msg(), None),
            TrashError::NoTrash("the disk it's on has no Trash".into())
        );
        // A write error isn't proof the disk has no Trash: macOS's own reason, and still the
        // question.
        let why = Some("You don't have permission to access it.".to_string());
        assert_eq!(
            classify_mac("NSCocoaErrorDomain", 513, true, msg(), why),
            TrashError::NoTrash("macOS couldn't move it there (You don't have permission to access it)".into())
        );
        assert_eq!(
            classify_mac("NSCocoaErrorDomain", 512, true, msg(), None),
            TrashError::NoTrash("macOS couldn't move it there (nope)".into())
        );
        // Its folder can't be changed: a delete would fail too.
        assert_eq!(classify_mac("NSCocoaErrorDomain", 3328, false, msg(), None), TrashError::Failed(msg()));
        // Not there any more, or another domain: just the error.
        assert_eq!(classify_mac("NSCocoaErrorDomain", 4, true, msg(), None), TrashError::Failed(msg()));
        assert_eq!(classify_mac("NSPOSIXErrorDomain", 3328, true, msg(), None), TrashError::Failed(msg()));
    }

    /// Only a fixed local disk goes on to the shell; everything else is refused up front.
    #[test]
    fn windows_asks_the_shell_only_about_fixed_local_disks() {
        assert_eq!(precheck(r"C:\", drive::FIXED), None);
        assert_eq!(precheck(r"C:\mnt\data\", drive::FIXED), None);
        assert_eq!(precheck(r"\\?\Volume{1234}\", drive::FIXED), None);
        let no = |root: &str, kind: u32, says: &str| {
            let why = precheck(root, kind).unwrap_or_default();
            assert!(why.contains(says), "{root} {kind}: {why}");
        };
        no(r"E:\", drive::REMOVABLE, "drive E: is a removable drive");
        no(r"Z:\", drive::REMOTE, "drive Z: is a network location");
        no(r"\\nas\photos\", drive::REMOTE, "“\\\\nas\\photos” is a network location");
        no(r"\\nas\photos\", drive::FIXED, "network location");
        no(r"\\?\UNC\nas\photos\", drive::FIXED, "network location");
        no(r"D:\", drive::CDROM, "disc");
        no(r"R:\", drive::RAMDISK, "RAM disk");
        no(r"Q:\", 0, "can't tell");
        no(r"Q:\", 1, "can't tell");
    }

    /// The shell's answers: recycled only when PostDeleteItem says the item landed in the bin.
    #[test]
    fn windows_verdicts_never_call_a_delete_a_recycle() {
        let at = "drive C:";
        assert_eq!(verdict(false, 0, Some((0, true)), at), Ok(()));
        // Refused before deleting: offer the confirm.
        assert!(matches!(verdict(true, 0x80004004_u32 as i32, None, at), Err(TrashError::NoTrash(w)) if w.contains("drive C:")));
        assert!(matches!(verdict(true, 0, Some((0, true)), at), Err(TrashError::NoTrash(_))));
        // Deleted, but not into the bin: said plainly.
        assert_eq!(verdict(false, 0, Some((0, false)), at), Err(TrashError::DeletedForGood));
        // Failures, and silence.
        assert!(
            matches!(verdict(false, 0, Some((0x80070005_u32 as i32, false)), at), Err(TrashError::Failed(w)) if w.contains("0x80070005"))
        );
        assert!(matches!(verdict(false, 0x80070002_u32 as i32, None, at), Err(TrashError::Failed(w)) if w.contains("0x80070002")));
        assert!(matches!(verdict(false, 0, None, at), Err(TrashError::Failed(_))));
    }

    /// On CI's Windows runner (a throwaway machine, so its Recycle Bin can take a scratch file):
    /// a file on the fixed system drive goes to the bin, and the same kind of file reached
    /// through the admin share (`\\localhost\C$\…`, a network path with no bin) is refused and
    /// left alone, both by the precheck and by the shell's own answer with the precheck skipped.
    #[cfg(windows)]
    #[test]
    fn windows_recycles_locally_and_refuses_a_network_path() {
        if std::env::var_os("CI").is_none() {
            eprintln!("skipped: only on CI (it uses the real Recycle Bin)");
            return;
        }
        let d = std::env::temp_dir().join(format!("arbor-recycle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let local = d.join("local.txt");
        std::fs::write(&local, "x").unwrap();
        assert_eq!(trash(&local), Ok(None));
        assert!(!local.exists());
        let f = d.join("remote.txt");
        std::fs::write(&f, "x").unwrap();
        let abs = std::fs::canonicalize(&f).unwrap();
        let s = abs.to_string_lossy();
        let s = s.trim_start_matches(r"\\?\");
        let unc = PathBuf::from(format!(r"\\localhost\{}${}", &s[..1], &s[2..]));
        // CI's runner shares C$; the test must reach it, or it proves nothing about the shell.
        assert!(unc.symlink_metadata().is_ok(), "{} isn't reachable", unc.display());
        let r = trash(&unc);
        assert!(matches!(&r, Err(TrashError::NoTrash(w)) if w.contains("network")), "{r:?}");
        // With the precheck skipped, the shell itself is asked, and the sink stops it.
        let root = r"\\localhost\C$\";
        let r = recycle(&unc, root);
        assert!(matches!(&r, Err(TrashError::NoTrash(_))), "the shell's own refusal: {r:?}");
        assert!(f.exists(), "never deleted");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    #[ignore = "moves a scratch file to the real Trash"]
    fn trashes_a_file() {
        let p = std::env::temp_dir().join(format!("arbor-trash-test-{}.txt", std::process::id()));
        std::fs::write(&p, "x").unwrap();
        let t = std::time::Instant::now();
        let r = super::trash(&p);
        eprintln!("trash took {:?}: {r:?}", t.elapsed());
        assert!(r.is_ok());
        assert!(!p.exists());
    }
}
