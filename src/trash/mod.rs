//! Moving files to the platform's trash. Returns where the item went when the platform says,
//! so a trash can be undone by moving it back.
//!
//! **A trash never silently becomes a permanent delete** (NORTHSTAR Safety #2). When an item's
//! disk has no trash that will take it, [`trash`] leaves it alone and says so
//! ([`TrashError::NoTrash`]); the UI then asks, in Satchel's one dialog, whether to delete it
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
    /// The operation was cancelled before anything happened to it (Windows: while its size was
    /// being added up). The item is untouched.
    #[cfg_attr(not(windows), allow(dead_code))]
    Cancelled,
}

impl std::fmt::Display for TrashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrashError::NoTrash(why) => write!(f, "can't go to the {BIN}: {why}"),
            TrashError::Failed(why) => f.write_str(why),
            TrashError::DeletedForGood => write!(f, "Windows deleted it permanently instead of putting it in the {BIN}"),
            TrashError::Cancelled => f.write_str("cancelled"),
        }
    }
}

// ---- macOS ------------------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub fn trash(path: &Path, _stop: &dyn Fn() -> bool) -> Result<Option<PathBuf>, TrashError> {
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
    let Ok(c) = crate::ffi::cstr(dir) else { return false };
    // SAFETY: `c` is a valid NUL-terminated path.
    unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

// ---- Linux and other freedesktop systems --------------------------------------------------------

/// The freedesktop.org trash spec (see the module docs).
#[cfg(all(unix, not(target_os = "macos")))]
pub fn trash(path: &Path, _stop: &dyn Fn() -> bool) -> Result<Option<PathBuf>, TrashError> {
    use std::os::unix::fs::MetadataExt;
    let home = home_trash();
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

/// The home trash: `$XDG_DATA_HOME/Trash` (`~/.local/share/Trash` by default). A path only.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn home_trash() -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| crate::fs::home().join(".local/share"));
    data.join("Trash")
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
    Err(TrashError::NoTrash(format!("“{}” has no trash Satchel can use{why}", top.display())))
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

/// macOS keeps no record beside the item, and Windows trash can't be undone from Satchel.
#[cfg(not(all(unix, not(target_os = "macos"))))]
pub fn forget(_trashed: &Path) {}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn url_escape(p: &Path) -> String {
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

/// Before anything else: only a local disk can have a Recycle Bin. Windows keeps none on
/// network shares (mapped drives and `\\server\share` paths), optical discs and RAM disks, and
/// deletes from them for good. A removable drive goes on to the registry's settings
/// ([`bin_verdict`]): most have no Recycle Bin, but one Windows keeps a bin for may be used.
/// `root` is the volume's root as `GetVolumePathNameW` gives it (`E:\`, `\\server\share\`),
/// `kind` its `GetDriveTypeW`. Returns why it can't be recycled, or `None` to go on.
#[cfg(any(windows, test))]
pub fn precheck(root: &str, kind: u32) -> Option<String> {
    let name = root.trim_end_matches('\\');
    let place = if name.len() == 2 && name.ends_with(':') { format!("drive {name}") } else { format!("“{name}”") };
    match kind {
        drive::FIXED | drive::REMOVABLE if !root.starts_with(r"\\") || root.starts_with(r"\\?\") && !root.starts_with(r"\\?\UNC\") => None,
        drive::FIXED | drive::REMOVABLE | drive::REMOTE => Some(format!("{place} is a network location, which has no {BIN}")),
        drive::CDROM => Some(format!("{place} is a disc, which has no {BIN}")),
        drive::RAMDISK => Some(format!("{place} is a RAM disk, which has no {BIN}")),
        _ => Some(format!("Satchel can't tell whether {place} has a {BIN}")),
    }
}

/// What Windows' registry says about the Recycle Bin of one volume: the per-volume key
/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{GUID}`
/// (`NukeOnDelete`, and `MaxCapacity` in MB), the parent `BitBucket` key's `NukeOnDelete` for a
/// volume without its own, and the Explorer policies in HKCU and HKLM (`NoRecycleFiles`, and
/// `RecycleBinSize`, a percentage of each drive).
#[cfg(any(windows, test))]
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BinSettings {
    pub policy_off: bool,
    /// The `RecycleBinSize` policy: the most each drive's bin may hold, as a percentage of the
    /// drive (Microsoft's policy reference), not megabytes.
    pub policy_percent: Option<u64>,
    /// The volume's size in bytes, which the percentage is of (read only when it's set).
    pub volume_bytes: Option<u64>,
    /// The volume has a key of its own.
    pub volume_key: bool,
    pub nuke: Option<u32>,
    pub global_nuke: Option<u32>,
    pub max_mb: Option<u64>,
}

/// Whether an item of `size` bytes (`None`: it couldn't all be read) would go to the Recycle Bin
/// on `place`, as far as the registry says; the reason when it wouldn't, or when anything is
/// unknown. A bin that's full makes room by dropping its oldest items, so only its size limit
/// matters, not what's in it. The shell's own answer is still checked per item ([`recycle`]).
#[cfg(any(windows, test))]
pub fn bin_verdict(s: &BinSettings, size: Option<u64>, place: &str, removable: bool) -> Result<(), String> {
    if s.policy_off {
        return Err(format!("the {BIN} is turned off by a policy on this computer"));
    }
    if !s.volume_key {
        return Err(if removable {
            format!("{place} is a removable drive, and Windows keeps no {BIN} on it")
        } else {
            format!("Windows has no {BIN} settings for {place}, so Satchel can't be sure it would keep it")
        });
    }
    match s.nuke.or(s.global_nuke) {
        Some(0) => {}
        Some(_) => return Err(format!("the {BIN} on {place} is set to remove files immediately")),
        None => return Err(format!("Windows doesn't say whether the {BIN} on {place} keeps files")),
    }
    let policy = match (s.policy_percent, s.volume_bytes) {
        (None, _) => None,
        (Some(pct), Some(volume)) => Some((u128::from(volume) * u128::from(pct.min(100)) / 100) as u64),
        (Some(_), None) => return Err(format!("Windows doesn't say how big {place} is, and a policy sizes its {BIN} as a share of it")),
    };
    let max = match (s.max_mb.map(|m| m.saturating_mul(1 << 20)), policy) {
        (Some(a), Some(b)) => a.min(b),
        (Some(m), None) | (None, Some(m)) => m,
        (None, None) => return Err(format!("Windows doesn't say how big the {BIN} on {place} is")),
    };
    let Some(size) = size else { return Err(format!("Satchel couldn't read all of it to check that it fits in the {BIN}")) };
    if size > max {
        return Err(format!(
            "it's bigger ({}) than the {BIN} on {place} can hold ({})",
            crate::fs::fmt_size(size),
            crate::fs::fmt_size(max)
        ));
    }
    Ok(())
}

/// What the shell's per-item answers add up to (see [`recycle`]): `refused` when Satchel stopped
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

/// The Recycle Bin. Windows doesn't say where the item went (Satchel can't undo it; restore it
/// from the Recycle Bin), so this returns `None` on success.
///
/// Before the shell is asked, on this worker: the registry's settings for the item's volume and
/// the item's size (a walk that never follows links, junctions or mounted volumes, and stops
/// when `stop` says), so an item the bin wouldn't keep (turned off, too big, unknown) is never
/// handed to a delete that might be permanent. It's left for Satchel's own question instead.
#[cfg(windows)]
pub fn trash(path: &Path, stop: &dyn Fn() -> bool) -> Result<Option<PathBuf>, TrashError> {
    let abs = std::path::absolute(path).map_err(|e| TrashError::Failed(e.to_string()))?;
    let (root, kind) = win::volume(&abs);
    if let Some(why) = precheck(&root, kind) {
        return Err(TrashError::NoTrash(why));
    }
    let settings = win::bin_settings(&root);
    let size = match tree_size(&abs, stop) {
        Size::Stopped => return Err(TrashError::Cancelled),
        Size::Known(n) => Some(n),
        Size::Unreadable => None,
    };
    bin_verdict(&settings, size, &place_of(&root), kind == drive::REMOVABLE).map_err(TrashError::NoTrash)?;
    recycle(&abs, &root)
}

/// `drive C:` or `“\\server\share”`, from a volume root.
#[cfg(any(windows, test))]
fn place_of(root: &str) -> String {
    let name = root.trim_end_matches('\\');
    if name.len() == 2 && name.ends_with(':') { format!("drive {name}") } else { format!("“{name}”") }
}

#[cfg(any(windows, test))]
#[derive(Debug, PartialEq)]
enum Size {
    Known(u64),
    Unreadable,
    Stopped,
}

/// The bytes in `p`'s files, not following links, junctions or volumes mounted in folders
/// (they aren't recycled with it), checking `stop` before each entry.
#[cfg(any(windows, test))]
fn tree_size(p: &Path, stop: &dyn Fn() -> bool) -> Size {
    fn walk(p: &Path, stop: &dyn Fn() -> bool, n: &mut u64) -> Result<(), Size> {
        if stop() {
            return Err(Size::Stopped);
        }
        let m = p.symlink_metadata().map_err(|_| Size::Unreadable)?;
        if m.file_type().is_dir() {
            for de in std::fs::read_dir(p).map_err(|_| Size::Unreadable)? {
                walk(&de.map_err(|_| Size::Unreadable)?.path(), stop, n)?;
            }
        } else if m.file_type().is_file() {
            *n += m.len();
        }
        Ok(())
    }
    let mut n = 0;
    match walk(p, stop, &mut n) {
        Ok(()) => Size::Known(n),
        Err(s) => s,
    }
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
/// warning, and its warning is Windows' own dialog, not Satchel's.
#[cfg(windows)]
fn recycle(abs: &Path, root: &str) -> Result<Option<PathBuf>, TrashError> {
    let r = win::recycle(abs)?;
    verdict(r.refused, r.performed, r.deleted, &place_of(root)).map(|()| None)
}

#[cfg(windows)]
mod win;

pub mod contents;

#[cfg(test)]
mod tests;
