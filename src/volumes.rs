//! The sidebar's "Locations": mounted volumes, and ejecting them.
//!
//! - **macOS:** what's mounted in `/Volumes`, without the boot volume's alias (a link to `/`)
//!   or a folder left behind with nothing mounted on it.
//! - **Linux:** the user's mounts from `/proc/self/mounts`: under `/media/$USER`,
//!   `/run/media/$USER`, `/media` and `/mnt` (one level down), not the kernel's own file
//!   systems.
//! - **Windows:** the drive letters (one bitmask call), each drive's type, and its label
//!   except for network drives (asking a disconnected one can take seconds).
//!
//! Read on a worker, never on the window's thread: at startup (after the first frame's
//! listing, not before it), and again only when asked: opening a sidebar item's context menu,
//! ⌘R, after an eject, and when the OS says a volume came or went where it can say so without
//! polling (macOS's NSWorkspace mount notifications, Windows' `WM_DEVICECHANGE` to a hidden
//! window of Arbor's own; Rune passes on neither, see RUNE-GAPS.md). Linux has no such
//! notification short of watching, so there it's manual.
//!
//! **Eject** runs on a worker and says what happened: macOS asks NSWorkspace
//! (`unmountAndEjectDeviceAtURL:`), Linux `udisksctl` (unmount, then power off) or `gio mount
//! -e`, Windows locks, dismounts and ejects the volume (`DeviceIoControl`), which fails while
//! another app has a file open there. A pane showing the volume goes to the home folder first.

use std::path::{Path, PathBuf};

use crate::app::State;
use crate::folder::Nav;

/// One mounted volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    pub name: String,
    pub path: PathBuf,
    /// Eject is offered (removable, a disk image, a network share on macOS; a udisks mount on
    /// Linux).
    pub eject: bool,
    /// Linux: the device it's mounted from (`/dev/sdb1`), for `udisksctl`.
    pub device: Option<String>,
    /// What tells this volume from another mounted at the same place later ([`identity`]),
    /// read with the list for those that can be ejected. Eject checks it again right before
    /// acting, so a refresh or a swapped disk never redirects it to another volume.
    pub id: Option<String>,
}

/// The volumes mounted now. Touches the disk: call on a worker.
pub fn list() -> Vec<Volume> {
    #[cfg(target_os = "macos")]
    let mut v = mac_list();
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut v = {
        let user = user_name();
        std::fs::read_to_string("/proc/self/mounts").map(|t| from_mounts(&t, &user)).unwrap_or_default()
    };
    #[cfg(windows)]
    let mut v = win::list();
    // Only those that can be ejected: a dead network mount's `stat` could hang.
    for vol in v.iter_mut().filter(|v| v.eject) {
        vol.id = identity(&vol.path);
    }
    v
}

/// What tells the volume mounted at `path` from another mounted there later: its device number
/// and what it's mounted from (Linux: the mount table's source; macOS: `statfs`'s
/// `f_mntfromname`), or on Windows the volume's GUID path. `None` when it can't be read.
/// Touches the disk: call on a worker.
pub fn identity(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = std::fs::metadata(path).ok()?.dev();
        #[cfg(target_os = "macos")]
        let from = {
            let c = crate::ffi::cstr(path).ok()?;
            // SAFETY: a NUL-terminated path and a zeroed out-buffer.
            let mut s: libc::statfs = unsafe { std::mem::zeroed() };
            if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
                return None;
            }
            s.f_mntfromname.iter().take_while(|&&c| c != 0).map(|&c| c as u8 as char).collect::<String>()
        };
        #[cfg(not(target_os = "macos"))]
        let from = std::fs::read_to_string("/proc/self/mounts").ok().and_then(|t| mount_source(&t, path)).unwrap_or_default();
        Some(format!("{dev} {from}"))
    }
    #[cfg(windows)]
    {
        win::volume_guid(path)
    }
}

/// The source of the last mount at `at` in a `/proc/self/mounts` text (a later mount on the same
/// point hides the earlier one).
#[cfg_attr(not(any(test, all(unix, not(target_os = "macos")))), allow(dead_code))]
pub fn mount_source(text: &str, at: &Path) -> Option<String> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.next()?, f.next()?))
        })
        .rfind(|(_, on)| Path::new(&unescape_mount(on)) == at)
        .map(|(dev, _)| unescape_mount(dev))
}

// ---- Linux --------------------------------------------------------------------------------

/// The user's name, for `/media/$USER`.
#[cfg(all(unix, not(target_os = "macos")))]
fn user_name() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| crate::fs::home().file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
}

#[cfg_attr(not(any(test, all(unix, not(target_os = "macos")))), allow(dead_code))]
/// Kernel and system file systems that are never a place for files of one's own.
const PSEUDO: &[&str] = &[
    "autofs",
    "binfmt_misc",
    "bpf",
    "cgroup",
    "cgroup2",
    "configfs",
    "debugfs",
    "devpts",
    "devtmpfs",
    "efivarfs",
    "fusectl",
    "hugetlbfs",
    "mqueue",
    "nsfs",
    "proc",
    "pstore",
    "securityfs",
    "sysfs",
    "tracefs",
];

/// A field of `/proc/self/mounts`, its escapes undone ([`crate::mounts::unescape_bytes`]).
#[cfg_attr(not(any(test, all(unix, not(target_os = "macos")))), allow(dead_code))]
pub fn unescape_mount(s: &str) -> String {
    String::from_utf8_lossy(&crate::mounts::unescape_bytes(s)).into_owned()
}

/// The user-visible volumes in a `/proc/self/mounts` text, in mount order: one level under
/// `/media/<user>`, `/run/media/<user>`, `/media` and `/mnt`, skipping kernel file systems. A
/// later mount on the same point hides the earlier one. Ejectable: a block device mounted under
/// `/media` or `/run/media` (where udisks mounts what's plugged in).
#[cfg_attr(not(any(test, all(unix, not(target_os = "macos")))), allow(dead_code))]
pub fn from_mounts(text: &str, user: &str) -> Vec<Volume> {
    let mut out: Vec<Volume> = Vec::new();
    let bases = [format!("/media/{user}/"), format!("/run/media/{user}/"), "/media/".to_string(), "/mnt/".to_string()];
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let (Some(dev), Some(at), Some(kind)) = (f.next(), f.next(), f.next()) else { continue };
        if PSEUDO.contains(&kind) {
            continue;
        }
        let (dev, at) = (unescape_mount(dev), unescape_mount(at));
        // One level down from a base, and not the user's own folder under /media.
        let under = bases.iter().find_map(|b| at.strip_prefix(b.as_str()).filter(|rest| !rest.is_empty() && !rest.contains('/')));
        let Some(name) = under else { continue };
        if at == format!("/media/{user}") {
            continue;
        }
        let eject = dev.starts_with("/dev/") && (at.starts_with("/media/") || at.starts_with("/run/media/"));
        out.retain(|v| v.path != Path::new(&at));
        out.push(Volume { name: name.to_string(), path: PathBuf::from(&at), eject, device: Some(dev), id: None });
    }
    out
}

/// Where `prog` is on the `PATH` (`path_var`), if anywhere.
#[cfg_attr(not(any(test, all(unix, not(target_os = "macos")))), allow(dead_code))]
pub fn find_program(prog: &str, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path_var?).map(|d| d.join(prog)).find(|p| p.is_file())
}

// ---- macOS --------------------------------------------------------------------------------

#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
/// One entry of `/Volumes`: its name, whether it's a link to `/` (the boot volume's alias),
/// and whether something is mounted there (its device differs from `/Volumes`'s).
pub struct VolumesEntry {
    pub name: String,
    pub root_link: bool,
    pub mounted: bool,
}

/// The volumes among `/Volumes`' entries: mounted ones, not the boot volume's alias, not
/// hidden (`.timemachine`) or the system's own (`com.apple.…`). All can be ejected (an
/// internal disk's other volumes refuse, and say so).
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub fn from_volumes_dir(entries: Vec<VolumesEntry>, dir: &Path) -> Vec<Volume> {
    let mut v: Vec<Volume> = entries
        .into_iter()
        .filter(|e| !e.root_link && e.mounted && !e.name.starts_with('.') && !e.name.starts_with("com.apple."))
        .map(|e| Volume { path: dir.join(&e.name), name: e.name, eject: true, device: None, id: None })
        .collect();
    v.sort_by_key(|v| v.name.to_lowercase());
    v
}

#[cfg(target_os = "macos")]
fn mac_list() -> Vec<Volume> {
    use std::os::unix::fs::MetadataExt;
    let dir = Path::new("/Volumes");
    let Ok(base) = std::fs::metadata(dir) else { return Vec::new() };
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let entries = rd
        .flatten()
        .map(|de| {
            let name = de.file_name().to_string_lossy().into_owned();
            let link = de.file_type().is_ok_and(|t| t.is_symlink());
            let root_link = link && std::fs::read_link(de.path()).is_ok_and(|t| t == Path::new("/"));
            let mounted = !link && de.metadata().is_ok_and(|m| m.dev() != base.dev());
            VolumesEntry { name, root_link, mounted }
        })
        .collect();
    from_volumes_dir(entries, dir)
}

// ---- Windows ------------------------------------------------------------------------------

/// `GetDriveTypeW`'s answers that Arbor tells apart.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub const DRIVE_REMOVABLE: u32 = 2;
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub const DRIVE_REMOTE: u32 = 4;
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub const DRIVE_CDROM: u32 = 5;

/// The drives in `GetLogicalDrives`' bitmask, each with its type and label (`label`: `None`
/// when it has none or wasn't asked). Removable drives and discs can be ejected.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub fn from_drives(mask: u32, kind: impl Fn(char) -> u32, label: impl Fn(char, u32) -> Option<String>) -> Vec<Volume> {
    ('A'..='Z')
        .enumerate()
        .filter(|(i, _)| mask & (1 << i) != 0)
        .map(|(_, l)| {
            let k = kind(l);
            let name = match label(l, k).filter(|s| !s.trim().is_empty()) {
                Some(s) => format!("{} ({l}:)", s.trim()),
                None => format!("{l}:"),
            };
            Volume {
                name,
                path: PathBuf::from(format!("{l}:\\")),
                eject: matches!(k, DRIVE_REMOVABLE | DRIVE_CDROM),
                device: None,
                id: None,
            }
        })
        .collect()
}

#[cfg(windows)]
pub(crate) mod win {
    use super::{DRIVE_REMOTE, Volume, from_drives};
    use windows_sys::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, GetVolumeNameForVolumeMountPointW,
    };

    use crate::ffi::wide;

    /// The volume's GUID path (`\\?\Volume{…}\`) for a drive root or folder mount point.
    pub fn volume_guid(root: &std::path::Path) -> Option<String> {
        let mut at = root.as_os_str().to_owned();
        if !root.to_string_lossy().ends_with('\\') {
            at.push("\\");
        }
        let mut name = [0u16; 64];
        // SAFETY: a NUL-terminated mount point and a buffer of the length given.
        let ok = unsafe { GetVolumeNameForVolumeMountPointW(wide(&at).as_ptr(), name.as_mut_ptr(), name.len() as u32) };
        if ok == 0 {
            return None;
        }
        let n = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        Some(String::from_utf16_lossy(&name[..n]))
    }

    pub fn list() -> Vec<Volume> {
        // A card reader or disc drive with nothing in it answers "not ready" rather than asking
        // the user to insert a disk (this thread only).
        // SAFETY: sets this thread's error mode; the old one isn't needed.
        unsafe {
            windows_sys::Win32::System::Diagnostics::Debug::SetThreadErrorMode(
                windows_sys::Win32::System::Diagnostics::Debug::SEM_FAILCRITICALERRORS,
                std::ptr::null_mut(),
            )
        };
        // SAFETY: takes no arguments and returns a bitmask.
        let mask = unsafe { GetLogicalDrives() };
        from_drives(
            mask,
            // SAFETY: a NUL-terminated root path.
            |l| unsafe { GetDriveTypeW(wide(format!("{l}:\\")).as_ptr()) },
            |l, kind| {
                if kind == DRIVE_REMOTE {
                    return None;
                }
                let mut name = [0u16; 261];
                // SAFETY: a NUL-terminated root, a buffer of the length given, the rest null.
                let ok = unsafe {
                    GetVolumeInformationW(
                        wide(format!("{l}:\\")).as_ptr(),
                        name.as_mut_ptr(),
                        name.len() as u32,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        0,
                    )
                };
                if ok == 0 {
                    return None;
                }
                let n = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                Some(String::from_utf16_lossy(&name[..n]))
            },
        )
    }
}

// ---- eject --------------------------------------------------------------------------------

/// Why an eject didn't happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EjectError {
    /// Another app has something open on it.
    InUse,
    /// It isn't the volume that was picked any more (another was mounted there since, or its
    /// identity can't be read): nothing was done.
    Changed,
    /// Unmounted, but the drive couldn't be powered off (Linux): it's not safe to pull yet.
    #[cfg_attr(any(test, not(all(unix, not(target_os = "macos")))), allow(dead_code))]
    NotPoweredOff(String),
    #[cfg_attr(test, allow(dead_code))]
    Failed(String),
}

/// What an eject says in the status bar.
pub fn eject_note(name: &str, r: &Result<(), EjectError>) -> String {
    match r {
        Ok(()) => format!("Ejected “{name}”"),
        Err(EjectError::InUse) => format!("“{name}” is in use by another app, so it wasn't ejected"),
        Err(EjectError::Changed) => format!("“{name}” changed since; refresh and try again"),
        Err(EjectError::NotPoweredOff(e)) => format!("Unmounted “{name}”, but it couldn't be powered off: {e}"),
        Err(EjectError::Failed(e)) => format!("Couldn't eject “{name}”: {e}"),
    }
}

/// Whether a tool's error output says the volume is busy.
#[cfg_attr(not(any(unix, test)), allow(dead_code))]
pub fn says_busy(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("busy") || t.contains("in use") || t.contains("dissented")
}

/// Ejects `v`, if it's still the volume that was picked ([`Volume::id`], checked right before
/// acting). Blocks until the OS answers: call on a worker.
pub fn eject(v: &Volume) -> Result<(), EjectError> {
    if rune::tooling::headless_run().is_some() {
        return Ok(());
    }
    if v.id.is_none() || identity(&v.path) != v.id {
        return Err(EjectError::Changed);
    }
    #[cfg(test)]
    return hook::eject(v);
    #[cfg(all(not(test), target_os = "macos"))]
    return mac_eject(&v.path);
    #[cfg(all(not(test), unix, not(target_os = "macos")))]
    return linux_eject(v, std::env::var_os("PATH").as_deref());
    #[cfg(all(not(test), windows))]
    return win_eject(&v.path);
}

/// Test only: what was ejected, instead of ejecting it.
#[cfg(test)]
pub(crate) mod hook {
    use std::sync::Mutex;

    pub(crate) static EJECTED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    pub(super) fn eject(v: &super::Volume) -> Result<(), super::EjectError> {
        EJECTED.lock().unwrap().push(v.name.clone());
        Ok(())
    }
}

/// udisks (unmount, then power off), else `gio mount -e`, found in `path` (`$PATH`).
#[cfg(all(unix, not(target_os = "macos")))]
#[cfg_attr(test, allow(dead_code))]
fn linux_eject(v: &Volume, path: Option<&std::ffi::OsStr>) -> Result<(), EjectError> {
    use std::process::Command;
    let run = |c: &mut Command| -> Result<(), EjectError> {
        let out = c.output().map_err(|e| EjectError::Failed(e.to_string()))?;
        if out.status.success() {
            return Ok(());
        }
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if says_busy(&err) {
            Err(EjectError::InUse)
        } else {
            Err(EjectError::Failed(err.lines().last().unwrap_or("it failed").to_string()))
        }
    };
    let dev = v.device.as_deref().filter(|d| d.starts_with("/dev/"));
    if let (Some(udisks), Some(dev)) = (find_program("udisksctl", path), dev) {
        run(Command::new(&udisks).args(["unmount", "--no-user-interaction", "-b", dev]))?;
        // Powering the drive off is what makes it safe to pull; a drive with another volume
        // still mounted says no, and the unmount stands, said as such.
        return match run(Command::new(&udisks).args(["power-off", "--no-user-interaction", "-b", dev])) {
            Ok(()) => Ok(()),
            Err(EjectError::InUse) => Err(EjectError::NotPoweredOff("another volume on the drive is in use".into())),
            Err(EjectError::Failed(e) | EjectError::NotPoweredOff(e)) => Err(EjectError::NotPoweredOff(e)),
            Err(EjectError::Changed) => Err(EjectError::Changed),
        };
    }
    if let Some(gio) = find_program("gio", path) {
        return run(Command::new(gio).args(["mount", "-e"]).arg(&v.path));
    }
    Err(EjectError::Failed("ejecting needs udisksctl (udisks2) or gio, and neither is installed".into()))
}

#[cfg(target_os = "macos")]
#[cfg_attr(test, allow(dead_code))]
fn mac_eject(path: &Path) -> Result<(), EjectError> {
    use objc2::rc::autoreleasepool;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;
    autoreleasepool(|_| {
        // SAFETY: NSWorkspace may be used from any thread; every object is checked for nil,
        // and the error is an autoreleased NSError or nil.
        unsafe {
            let ws: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
            let Some(ws) = ws.as_ref() else { return Err(EjectError::Failed("NSWorkspace isn't available".into())) };
            let p = NSString::from_str(&path.to_string_lossy());
            let url: *mut AnyObject = msg_send![class!(NSURL), fileURLWithPath: &*p];
            let mut err: *mut AnyObject = std::ptr::null_mut();
            let ok: bool = msg_send![ws, unmountAndEjectDeviceAtURL: url, error: &mut err];
            if ok {
                return Ok(());
            }
            let Some(e) = err.as_ref() else { return Err(EjectError::Failed("macOS didn't say why".into())) };
            let code: isize = msg_send![e, code];
            let text: *mut NSString = msg_send![e, localizedDescription];
            let text = text.as_ref().map(|t| t.to_string()).unwrap_or_default();
            // fBsyErr (-47), EBUSY, or the words.
            if code == -47 || code == 16 || says_busy(&text) { Err(EjectError::InUse) } else { Err(EjectError::Failed(text)) }
        }
    })
}

#[cfg(windows)]
#[cfg_attr(test, allow(dead_code))]
fn win_eject(path: &Path) -> Result<(), EjectError> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, GENERIC_READ, GENERIC_WRITE, GetLastError, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, IOCTL_STORAGE_EJECT_MEDIA, IOCTL_STORAGE_MEDIA_REMOVAL, PREVENT_MEDIA_REMOVAL,
    };
    let letter = path.to_string_lossy().chars().next().unwrap_or('?');
    let dev: Vec<u16> = format!("\\\\.\\{letter}:").encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: a NUL-terminated device path; the handle is closed below on every path.
    let h = unsafe {
        CreateFileW(
            dev.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(EjectError::Failed(std::io::Error::last_os_error().to_string()));
    }
    let ioctl = |code: u32, input: *const core::ffi::c_void, len: u32| -> bool {
        let mut got = 0u32;
        // SAFETY: an open volume handle; `input` is null or points at `len` bytes.
        unsafe { DeviceIoControl(h, code, input, len, std::ptr::null_mut(), 0, &mut got, std::ptr::null_mut()) != 0 }
    };
    let result = (|| {
        if !ioctl(FSCTL_LOCK_VOLUME, std::ptr::null(), 0) {
            // SAFETY: reads this thread's last error.
            let e = unsafe { GetLastError() };
            return Err(if e == ERROR_ACCESS_DENIED || e == ERROR_SHARING_VIOLATION {
                EjectError::InUse
            } else {
                EjectError::Failed(std::io::Error::from_raw_os_error(e as i32).to_string())
            });
        }
        if !ioctl(FSCTL_DISMOUNT_VOLUME, std::ptr::null(), 0) {
            return Err(EjectError::Failed(std::io::Error::last_os_error().to_string()));
        }
        let allow = PREVENT_MEDIA_REMOVAL { PreventMediaRemoval: 0 };
        ioctl(
            IOCTL_STORAGE_MEDIA_REMOVAL,
            (&allow as *const PREVENT_MEDIA_REMOVAL).cast(),
            std::mem::size_of::<PREVENT_MEDIA_REMOVAL>() as u32,
        );
        if !ioctl(IOCTL_STORAGE_EJECT_MEDIA, std::ptr::null(), 0) {
            return Err(EjectError::Failed(std::io::Error::last_os_error().to_string()));
        }
        Ok(())
    })();
    // SAFETY: the handle opened above.
    unsafe { CloseHandle(h) };
    result
}

// ---- notifications ------------------------------------------------------------------------

/// Starts listening for volumes coming and going, where the OS tells without polling: macOS's
/// NSWorkspace notifications and Windows' `WM_DEVICECHANGE`. Each refreshes the list.
pub fn watch(handle: rune::AppHandle<State>) {
    let refresh: Refresh = std::sync::Arc::new(move || {
        handle.update(|s, tx| {
            tx.instant();
            s.refresh_volumes();
        });
    });
    #[cfg(target_os = "macos")]
    mac_watch(refresh);
    #[cfg(windows)]
    win_watch(refresh);
    #[cfg(all(unix, not(target_os = "macos")))]
    drop(refresh);
}

type Refresh = std::sync::Arc<dyn Fn() + Send + Sync>;

#[cfg(target_os = "macos")]
fn mac_watch(refresh: Refresh) {
    use objc2::rc::autoreleasepool;
    use objc2::runtime::{AnyClass, AnyObject, ClassBuilder, NSObject, Sel};
    use objc2::{ClassType, class, msg_send, sel};
    use objc2_foundation::NSString;
    use std::sync::OnceLock;
    static REFRESH: OnceLock<Refresh> = OnceLock::new();
    if REFRESH.set(refresh).is_err() {
        return;
    }
    extern "C" fn changed(_: &AnyObject, _: Sel, _note: *mut AnyObject) {
        if let Some(r) = REFRESH.get() {
            r();
        }
    }
    const NAME: &std::ffi::CStr = c"ArborVolumeWatcher";
    let Some(class) = AnyClass::get(NAME).or_else(|| {
        let mut b = ClassBuilder::new(NAME, NSObject::class())?;
        // SAFETY: the function matches the selector (an object in, void out).
        unsafe { b.add_method(sel!(volumesChanged:), changed as extern "C" fn(_, _, _)) };
        Some(b.register())
    }) else {
        return;
    };
    // SAFETY: `new` on an NSObject subclass (kept for the process); NSWorkspace's own
    // notification center delivers on the main thread.
    autoreleasepool(|_| unsafe {
        let made: *mut AnyObject = msg_send![class, new];
        let ws: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        let Some(ws) = ws.as_ref() else { return };
        let center: *mut AnyObject = msg_send![ws, notificationCenter];
        let Some(center) = center.as_ref() else { return };
        let nil: *mut AnyObject = std::ptr::null_mut();
        for name in ["NSWorkspaceDidMountNotification", "NSWorkspaceDidUnmountNotification", "NSWorkspaceDidRenameVolumeNotification"] {
            let name = NSString::from_str(name);
            let _: () = msg_send![center, addObserver: made, selector: sel!(volumesChanged:), name: &*name, object: nil];
        }
    });
}

#[cfg(windows)]
fn win_watch(refresh: Refresh) {
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG, RegisterClassW, TranslateMessage, WM_DEVICECHANGE, WNDCLASSW,
    };
    static REFRESH: OnceLock<Refresh> = OnceLock::new();
    if REFRESH.set(refresh).is_err() {
        return;
    }
    /// `DBT_DEVICEARRIVAL` and `DBT_DEVICEREMOVECOMPLETE`.
    const ARRIVED: usize = 0x8000;
    const REMOVED: usize = 0x8004;
    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        if msg == WM_DEVICECHANGE
            && (w == ARRIVED || w == REMOVED)
            && let Some(r) = REFRESH.get()
        {
            r();
        }
        // SAFETY: the arguments Windows passed.
        unsafe { DefWindowProcW(hwnd, msg, w, l) }
    }
    // A hidden top-level window (message-only windows don't get the broadcast), on a thread of
    // its own that sleeps in GetMessage: no polling.
    let _ = std::thread::Builder::new().name("arbor-devices".into()).spawn(|| {
        let class: Vec<u16> = "ArborDeviceWatcher\0".encode_utf16().collect();
        // SAFETY: a class and window of this thread's own, with a static window procedure; the
        // loop ends when GetMessage fails or the process exits.
        unsafe {
            let inst = GetModuleHandleW(std::ptr::null());
            let wc = WNDCLASSW { lpfnWndProc: Some(proc), hInstance: inst, lpszClassName: class.as_ptr(), ..std::mem::zeroed() };
            if RegisterClassW(&wc) == 0 {
                return;
            }
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                inst,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return;
            }
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    });
}

// ---- the UI side --------------------------------------------------------------------------

impl State {
    /// The ejectable volume the active pane's folder is on, if any (the innermost).
    pub fn volume_here(&self) -> Option<usize> {
        let cwd = &self.cur().cwd;
        self.volumes
            .iter()
            .enumerate()
            .filter(|(_, v)| v.eject && cwd.starts_with(&v.path))
            .max_by_key(|(_, v)| v.path.as_os_str().len())
            .map(|(i, _)| i)
    }

    /// Reads the volumes again, on a worker (the list on screen stays until it lands).
    pub fn refresh_volumes(&mut self) {
        self.volume_loader.request((), |()| list());
    }

    /// Ejects volume `v` (index into `volumes`) on a worker. A pane showing it goes home first.
    /// The volume mounted at `path` now (the ⏏ beside a row, which knows its path).
    pub fn eject_at(&mut self, path: &Path) {
        if let Some(vol) = self.volumes.iter().find(|v| v.path == path).cloned() {
            self.eject(vol);
        }
    }

    /// Ejects `vol` (as the list had it when it was picked) on a worker, which checks it's
    /// still that volume first. Refused while a file operation reads or writes there. A pane
    /// showing it goes home first, and searches that could walk into it stop.
    pub fn eject(&mut self, vol: Volume) {
        if self.ejecting.is_some() {
            self.note = Some("Already ejecting a volume".into());
            return;
        }
        if self.job.as_ref().is_some_and(|j| j.touches(&vol.path)) {
            self.note = Some(format!("A file operation is using “{}”: let it finish or cancel it (Esc), then eject", vol.name));
            return;
        }
        for p in &mut self.panes {
            if let Some(s) = p.f.search.as_mut()
                && vol.path.starts_with(&p.f.cwd)
            {
                s.stop_all();
            }
        }
        let home = crate::fs::home();
        for ix in 0..self.panes.len() {
            let on = {
                let f = &self.panes[ix].f;
                f.cwd.starts_with(&vol.path) || f.pending.as_ref().is_some_and(|p| p.path.starts_with(&vol.path))
            };
            if on {
                let was = self.active;
                self.active = ix;
                self.leave_search(ix, &mut rune::Tx::default());
                self.go(home.clone(), Nav::New, None);
                self.active = was;
            }
        }
        if self.preview.as_ref().and_then(|p| p.target.as_ref()).is_some_and(|t| t.dir.starts_with(&vol.path)) {
            self.close_preview();
        }
        if self.info.as_ref().is_some_and(|i| i.paths.iter().any(|p| p.starts_with(&vol.path))) {
            self.close_info();
        }
        self.note = Some(format!("Ejecting “{}”…", vol.name));
        self.ejecting = Some(vol.name.clone());
        self.eject_job.request(vol, |vol| {
            let r = eject(&vol);
            (vol.name, r)
        });
    }

    /// Volumes read and ejects finished since, from `poll`. True when anything changed.
    pub(crate) fn poll_volumes(&mut self) -> bool {
        let mut changed = false;
        if let Some(v) = self.volume_loader.take()
            && v != self.volumes
        {
            self.volumes = v;
            changed = true;
        }
        if let Some((name, r)) = self.eject_job.take() {
            self.ejecting = None;
            self.note = Some(eject_note(&name, &r));
            self.alert = r.is_err().then(|| self.note.clone()).flatten();
            self.refresh_volumes();
            changed = true;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTS: &str = "\
sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0
proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0
/dev/nvme0n1p2 / ext4 rw,relatime 0 0
tmpfs /run tmpfs rw,nosuid,nodev 0 0
/dev/sdb1 /media/smr/USB\\040Stick vfat rw,nosuid,nodev,relatime 0 0
/dev/sdc1 /run/media/smr/Backup ext4 rw,nosuid,nodev 0 0
nas:/export /mnt/nas nfs4 rw,relatime 0 0
/dev/sdd1 /mnt/data/deeper ext4 rw 0 0
systemd-1 /mnt/auto autofs rw 0 0
/dev/sde1 /media/smr/Old ext4 rw 0 0
/dev/sdf1 /media/smr/Old ext4 rw 0 0
/dev/loop3 /snap/core/1 squashfs ro 0 0
/dev/sdg1 /media/cdrom iso9660 ro 0 0
";

    #[test]
    fn the_last_mount_at_a_point_is_its_source() {
        assert_eq!(mount_source(MOUNTS, Path::new("/media/smr/Old")).as_deref(), Some("/dev/sdf1"));
        assert_eq!(mount_source(MOUNTS, Path::new("/media/smr/USB Stick")).as_deref(), Some("/dev/sdb1"));
        assert_eq!(mount_source(MOUNTS, Path::new("/media/smr")), None);
    }

    fn eject_state(name: &str, vols: &[&str]) -> (State, Vec<Volume>) {
        let d = std::env::temp_dir().join(format!("arbor-eject-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let vols: Vec<Volume> = vols
            .iter()
            .map(|v| {
                let path = d.join(v);
                std::fs::create_dir_all(&path).unwrap();
                Volume { name: format!("{name}-{v}"), id: identity(&path), path, eject: true, device: None }
            })
            .collect();
        let mut s = crate::app::tests::state_in(&d);
        crate::app::tests::settle_all(&mut s);
        s.places.clear();
        s.volumes = vols.clone();
        (s, vols)
    }

    fn settle_eject(s: &mut State) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while s.ejecting.is_some() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(2));
            s.poll_volumes();
        }
        assert!(s.ejecting.is_none(), "the eject answered");
    }

    fn ejected(name: &str) -> bool {
        hook::EJECTED.lock().unwrap().iter().any(|n| n == name)
    }

    /// The reviewer's case: USB's menu is open, a refresh drops USB from the list, and Eject is
    /// chosen: Backup, now at USB's row, must not be ejected.
    #[test]
    fn a_refresh_while_the_menu_is_open_never_retargets_eject() {
        let (mut s, vols) = eject_state("menu", &["USB", "Backup"]);
        let mut tx = rune::Tx::default();
        s.sidebar_menu(0, rune::PointerPress { x: 1.0, y: 1.0 }, &mut tx);
        s.volumes.remove(0);
        s.run_act(crate::menu::Act::Place(crate::menu::PlaceAct::Eject, 0), &mut tx);
        settle_eject(&mut s);
        assert!(!ejected(&vols[1].name), "clicked USB, ejected Backup");
        assert!(ejected(&vols[0].name), "USB itself, still mounted, is ejected");
    }

    /// Another volume mounted at the picked one's place since: nothing is ejected, and it says so.
    #[test]
    fn a_volume_that_changed_since_it_was_picked_is_left_alone() {
        let (mut s, mut vols) = eject_state("changed", &["USB"]);
        vols[0].id = Some("another disk".into());
        s.eject(vols[0].clone());
        settle_eject(&mut s);
        assert!(!ejected(&vols[0].name));
        assert_eq!(s.note.as_deref(), Some(format!("“{}” changed since; refresh and try again", vols[0].name).as_str()));
    }

    /// A file operation reading from (or writing to) the volume holds Eject off.
    #[test]
    fn eject_waits_for_a_file_operation_on_the_volume() {
        let (mut s, vols) = eject_state("busy", &["USB"]);
        std::fs::write(vols[0].path.join("f"), "x").unwrap();
        let elsewhere = std::env::temp_dir().join(format!("arbor-eject-busy-dest-{}", std::process::id()));
        std::fs::create_dir_all(&elsewhere).unwrap();
        s.job = Some(crate::ops::start(crate::ops::Op::Copy { sources: vec![vols[0].path.join("f")], dest: elsewhere }, None, None));
        s.eject(vols[0].clone());
        assert!(s.ejecting.is_none(), "not started");
        assert!(s.note.as_deref().is_some_and(|n| n.starts_with("A file operation is using")), "{:?}", s.note);
    }

    /// udisks unmounts but can't power the drive off: said as such, not "Ejected".
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_drive_that_unmounts_but_wont_power_off_says_so() {
        use std::os::unix::fs::PermissionsExt;
        let bin = std::env::temp_dir().join(format!("arbor-eject-udisks-{}", std::process::id()));
        std::fs::create_dir_all(&bin).unwrap();
        let tool = bin.join("udisksctl");
        std::fs::write(
            &tool,
            "#!/bin/sh\n[ \"$1\" = unmount ] && exit 0\necho 'Error powering off drive: not a removable device' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let v = Volume { name: "USB".into(), path: "/media/u/USB".into(), eject: true, device: Some("/dev/sdx1".into()), id: None };
        let r = linux_eject(&v, Some(bin.as_os_str()));
        assert_eq!(r, Err(EjectError::NotPoweredOff("Error powering off drive: not a removable device".into())));
        assert_eq!(
            eject_note("USB", &r),
            "Unmounted “USB”, but it couldn't be powered off: Error powering off drive: not a removable device"
        );
    }

    #[test]
    fn linux_lists_the_users_mounts_only() {
        let v = from_mounts(MOUNTS, "smr");
        let names: Vec<(&str, &str, bool)> = v.iter().map(|v| (v.name.as_str(), v.path.to_str().unwrap(), v.eject)).collect();
        assert_eq!(
            names,
            [
                ("USB Stick", "/media/smr/USB Stick", true),
                ("Backup", "/run/media/smr/Backup", true),
                ("nas", "/mnt/nas", false),
                ("Old", "/media/smr/Old", true),
                ("cdrom", "/media/cdrom", true),
            ]
        );
        assert_eq!(v[0].device.as_deref(), Some("/dev/sdb1"));
        assert_eq!(v[3].device.as_deref(), Some("/dev/sdf1"), "the later mount on a point wins");
        assert!(from_mounts(MOUNTS, "someone").iter().all(|v| !v.path.starts_with("/media/someone")));
    }

    #[test]
    fn mount_escapes_are_undone() {
        assert_eq!(unescape_mount("/media/a\\040b\\011c\\134d"), "/media/a b\tc\\d");
        assert_eq!(unescape_mount("/x\\04"), "/x\\04", "a short escape stays as it is");
    }

    #[test]
    fn macos_skips_the_boot_alias_and_leftover_folders() {
        let e = |name: &str, root_link, mounted| VolumesEntry { name: name.into(), root_link, mounted };
        let v = from_volumes_dir(
            vec![
                e("Macintosh HD", true, false),
                e("USB", false, true),
                e("old", false, false),
                e(".timemachine", false, true),
                e("com.apple.TimeMachine.localsnapshots", false, true),
                e("archive", false, true),
            ],
            Path::new("/Volumes"),
        );
        let names: Vec<&str> = v.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["archive", "USB"]);
        assert_eq!(v[1].path, Path::new("/Volumes/USB"));
        assert!(v.iter().all(|v| v.eject));
    }

    #[test]
    fn windows_drives_with_types_and_labels() {
        let mask = 0b10101; // A, C, E
        let kind = |l: char| match l {
            'A' => DRIVE_REMOVABLE,
            'C' => 3,
            _ => DRIVE_REMOTE,
        };
        let label = |l: char, k: u32| (k != DRIVE_REMOTE && l == 'C').then(|| "Windows".to_string());
        let v = from_drives(mask, kind, label);
        let got: Vec<(&str, &str, bool)> = v.iter().map(|v| (v.name.as_str(), v.path.to_str().unwrap(), v.eject)).collect();
        assert_eq!(got, [("A:", "A:\\", true), ("Windows (C:)", "C:\\", false), ("E:", "E:\\", false)]);
    }

    #[test]
    fn eject_says_what_happened() {
        assert_eq!(eject_note("USB", &Ok(())), "Ejected “USB”");
        assert_eq!(eject_note("USB", &Err(EjectError::InUse)), "“USB” is in use by another app, so it wasn't ejected");
        assert!(says_busy("Error unmounting /dev/sdb1: target is busy"));
        assert!(says_busy("Volume USB on disk4s1 failed to eject: dissented by PID 312"));
        assert!(!says_busy("Not authorized"));
    }

    #[test]
    fn programs_are_found_on_the_path() {
        let d = std::env::temp_dir().join(format!("arbor-vol-path-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("udisksctl"), "").unwrap();
        let path = std::env::join_paths([Path::new("/nonexistent"), &d]).unwrap();
        assert_eq!(find_program("udisksctl", Some(&path)), Some(d.join("udisksctl")));
        assert_eq!(find_program("gio", Some(&path)), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
