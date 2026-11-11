//! New File and Make Symbolic Link: what they make, never over anything (`create_new`, and link
//! calls that fail on a taken name), and undoing and redoing the links.
//!
//! A link points at the item's absolute path as its folder was listed (not resolved): it keeps
//! working when the link alone is moved (to the Desktop, into another folder), which is what a
//! link made beside its item is for, as Finder's aliases do. A relative target would survive
//! moving the pair together instead. See DECISIONS.md.
//!
//! On Windows a symbolic link needs Developer Mode or an administrator (`std`'s link calls ask
//! `CreateSymbolicLinkW` with `SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE`); without either, or
//! on a drive with no links (FAT), Arbor makes a shell shortcut (`.lnk`) instead and says so
//! ([`shortcut_fallback`]).

use super::*;

/// Making a new file, as the status bar says it.
pub const NEW_FILE: &str = "Making a New File";

/// Making links, as the status bar says it.
pub const MAKING_LINKS: &str = "Making Links";

/// New File's name, before a free variant ("untitled 2.txt").
pub const UNTITLED_FILE: &str = "untitled.txt";

/// Whether a failed shortcut instead of a symbolic link is Arbor's answer here.
const SHORTCUTS: bool = cfg!(windows);

/// A link Arbor made: what undo checks before removing it, and what redo makes again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Link {
    /// What it points at: the item's absolute path.
    pub(super) target: PathBuf,
    /// The item was a folder (Windows makes and removes links to folders as folders).
    pub(super) dir: bool,
    /// A Windows shortcut (`.lnk`), not a symbolic link.
    pub(super) shortcut: bool,
}

/// Whether a symbolic link that failed with OS error `code` is made as a Windows shortcut
/// instead, and why, for the status bar. Windows error codes: `ERROR_PRIVILEGE_NOT_HELD` (not an
/// administrator and no Developer Mode), `ERROR_INVALID_FUNCTION` and `ERROR_NOT_SUPPORTED` (a
/// drive whose file system has no links, FAT and exFAT). Anything else is an error. Only asked
/// on Windows.
pub fn shortcut_fallback(code: Option<i32>) -> Option<&'static str> {
    const ERROR_INVALID_FUNCTION: i32 = 1;
    const ERROR_NOT_SUPPORTED: i32 = 50;
    const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;
    match code? {
        ERROR_PRIVILEGE_NOT_HELD => Some("Windows makes symbolic links only in Developer Mode or for administrators"),
        ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED => Some("this drive can't hold symbolic links"),
        _ => None,
    }
}

/// The name of try `n` (from 1) at a link to an item named `name`: "name link", "name link 2",
/// with " link" before a file's extension so the link keeps its item's type ("photo link.jpg"
/// opens, previews and shows its icon as a photo; Windows opens by extension). A Windows
/// shortcut adds ".lnk".
pub(super) fn link_name(name: &std::ffi::OsStr, dir: bool, n: usize, shortcut: bool) -> std::ffi::OsString {
    let num = if n <= 1 { String::new() } else { format!(" {n}") };
    let mut out = match name.to_str() {
        Some(s) => {
            let (stem, ext) = match s.rfind('.') {
                Some(i) if i > 0 && !dir => (&s[..i], &s[i..]),
                _ => (s, ""),
            };
            std::ffi::OsString::from(format!("{stem} link{num}{ext}"))
        }
        // Not Unicode: kept whole, the words after it.
        None => {
            let mut o = name.to_os_string();
            o.push(format!(" link{num}"));
            o
        }
    };
    if shortcut {
        out.push(".lnk");
    }
    out
}

/// The first free name for a link to `name` in `dir` (free when it looked: the link call
/// itself refuses a taken name).
fn free_link(dir: &Path, name: &std::ffi::OsStr, is_dir: bool, shortcut: bool) -> PathBuf {
    (1..).map(|n| dir.join(link_name(name, is_dir, n, shortcut))).find(|p| p.symlink_metadata().is_err()).expect("a free name")
}

/// Makes `link` at `at`, failing if anything is there.
pub(super) fn make_link_at(at: &Path, link: &Link) -> io::Result<()> {
    if link.shortcut { shortcut(&link.target, at) } else { make_symlink(&link.target, at, link.dir) }
}

/// A Windows shortcut at `at` to `target`, never over anything: the shell's `IPersistFile::Save`
/// replaces what's at its path, so it saves to a fresh name beside `at` (made with `create_new`
/// first), which then takes `at`'s name with the no-replace rename.
#[cfg(windows)]
fn shortcut(target: &Path, at: &Path) -> io::Result<()> {
    let tmp = noclobber::temp_sibling(at)?;
    std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let made = lnk::write(target, &tmp).and_then(|()| noclobber::rename(&tmp, at));
    if made.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    made
}

#[cfg(not(windows))]
fn shortcut(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::Error::new(ErrorKind::Unsupported, "shortcuts are Windows' own"))
}

impl Worker<'_> {
    /// New File: an empty file named "untitled.txt" (or a free variant) in `dir`, made with
    /// `create_new`, so a file another app makes at the name first is never opened or replaced:
    /// the next free name is tried.
    pub(super) fn new_file(&mut self, dir: &Path) -> io::Result<PathBuf> {
        let mut last = None;
        for _ in 0..TRIES {
            let path = free_name(dir, std::ffi::OsStr::new(UNTITLED_FILE), false);
            self.before_act(&path);
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(path),
                // Taken since `free_name` looked: try the next free name.
                Err(e) if e.kind() == ErrorKind::AlreadyExists => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::from(ErrorKind::AlreadyExists)))
    }

    /// Make Symbolic Link: a link beside each item, each item first checked to be the one
    /// chosen. One undo record for all of them.
    pub(super) fn make_links(&mut self, items: Vec<PathBuf>, out: &mut Outcome) {
        self.p.items_total.store(items.len(), Relaxed);
        let mut steps = Vec::new();
        let mut shortcuts = None;
        for src in &items {
            if self.cancelled() {
                break;
            }
            if let Some(why) = self.changed(src) {
                out.errors.push(why);
                self.p.items_done.fetch_add(1, Relaxed);
                continue;
            }
            match self.make_link(src) {
                Ok((at, link, why)) => {
                    if why.is_some() {
                        shortcuts = why;
                    }
                    match made_step(&at, link) {
                        Ok(step) => steps.push(step),
                        Err(e) => out.errors.push(format!("“{}” can't be undone: it couldn't be read right after ({e})", name_of(&at))),
                    }
                    out.results.push(at);
                }
                Err(e) => out.errors.push(format!("{}: {e}", name_of(src))),
            }
            self.p.items_done.fetch_add(1, Relaxed);
            self.tick(0);
        }
        if let Some(why) = shortcuts {
            out.notes.push(format!("Made as shortcuts (.lnk): {why}"));
        }
        if !steps.is_empty() {
            out.undo = Some(Undo { what: What::Link, redo: false, steps });
        }
    }

    /// One link beside `src`: where, what it is, and why it's a shortcut when it is.
    fn make_link(&mut self, src: &Path) -> Result<(PathBuf, Link, Option<&'static str>), String> {
        let (Some(dir), Some(name)) = (src.parent(), src.file_name()) else { return Err("it has no folder to put a link in".into()) };
        // Followed: a link to a link to a folder is a folder link on Windows. One that leads
        // nowhere links as a file.
        let is_dir = src.metadata().is_ok_and(|m| m.is_dir());
        let mut link = Link { target: src.to_path_buf(), dir: is_dir, shortcut: false };
        let mut why = None;
        for _ in 0..TRIES {
            let at = free_link(dir, name, is_dir, link.shortcut);
            self.before_act(&at);
            match make_link_at(&at, &link) {
                Ok(()) => return Ok((at, link, why)),
                // Taken since `free_link` looked: the next free name.
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => match shortcut_fallback(e.raw_os_error()).filter(|_| SHORTCUTS && !link.shortcut) {
                    Some(w) => {
                        link.shortcut = true;
                        why = Some(w);
                    }
                    None => return Err(format!("couldn't make a link ({e})")),
                },
            }
        }
        Err("couldn't make a link: every free name was taken first".into())
    }

    /// Undoes links Arbor made (removing each only while it's still the link made: identity,
    /// time, size and target), or redoes them (making each again where nothing has taken its
    /// name). All checked first, nothing changed unless all pass, as [`Worker::undo`] does.
    /// Removed, not trashed: a link holds no data, and redo makes it again exactly.
    pub(super) fn undo_links(&mut self, undo: Undo, out: &mut Outcome) {
        self.p.items_total.store(undo.steps.len(), Relaxed);
        let making = undo.redo;
        let problems: Vec<String> = undo.steps.iter().filter_map(|s| self.check_link(making, s).err()).collect();
        if self.cancelled() {
            out.undo = Some(undo);
            return;
        }
        if let Some(first) = problems.first() {
            let more = if problems.len() > 1 { format!(" (and {} more)", problems.len() - 1) } else { String::new() };
            out.refused = Some(format!("Can't {} the {}: {first}{more}. Nothing was changed.", undo.word(), undo.what.noun()));
            out.undo = Some(undo);
            return;
        }
        let Undo { what, redo, steps } = undo;
        let recheck = steps.len() > 1;
        let (mut left, mut inverse) = (Vec::new(), Vec::new());
        for step in steps {
            if self.cancelled() {
                left.push(step);
                continue;
            }
            if recheck && let Err(why) = self.check_link(making, &step) {
                out.errors.push(format!("{}: {why}", if making { "Not redone" } else { "Not undone" }));
                left.push(step);
                continue;
            }
            let Some(link) = step.link.clone() else {
                left.push(step);
                continue;
            };
            self.before_act(&step.at);
            let name = name_of(&step.at);
            let done = if making { make_link_at(&step.at, &link) } else { self.remove_link(&step, &link) };
            match done {
                Ok(()) if making => {
                    match made_step(&step.at, link) {
                        Ok(again) => inverse.push(again),
                        Err(e) => out.errors.push(format!("“{name}” can't be undone: it couldn't be read right after ({e})")),
                    }
                    out.results.push(step.at);
                }
                Ok(()) => {
                    out.removed.push(step.at.clone());
                    out.results.push(step.at.clone());
                    inverse.push(Step { print: unmatchable(), ident: None, ..step });
                }
                Err(e) if e.kind() == ErrorKind::Other && e.get_ref().is_some_and(|r| r.is::<Replaced>()) => {
                    out.errors.push(format!("“{name}” was replaced just now, so it was left alone"));
                    left.push(step);
                }
                Err(e) if making && e.kind() == ErrorKind::AlreadyExists => {
                    out.errors.push(format!("“{name}” was taken just now, so the link wasn't made again"));
                    left.push(step);
                }
                Err(e) => {
                    out.errors.push(format!("{name}: {e}"));
                    left.push(step);
                }
            }
            self.p.items_done.fetch_add(1, Relaxed);
            self.tick(0);
        }
        if !left.is_empty() {
            out.undo = Some(Undo { what, redo, steps: left });
        }
        if !inverse.is_empty() {
            out.inverse = Some(Undo { what, redo: !redo, steps: inverse });
        }
    }

    /// Whether a link step can go: to remove it, it must still be exactly the link Arbor made
    /// (fingerprint, and a symbolic link's target); to make it again, its name must be free and
    /// its folder there.
    fn check_link(&self, making: bool, step: &Step) -> Result<(), String> {
        let name = format!("“{}”", name_of(&step.at));
        let Some(link) = &step.link else { return Err(format!("{name} isn't a link Arbor made")) };
        if making {
            return match step.at.symlink_metadata() {
                Ok(_) => Err(format!("{name} is already taken, so the link can't be made again")),
                Err(e) if !is_gone(&e) => Err(format!("{name} can't be checked ({e})")),
                Err(_) if step.at.parent().is_none_or(|p| !p.is_dir()) => Err(format!("the folder {name} was in is gone")),
                Err(_) => Ok(()),
            };
        }
        match self.diff(&step.at, &step.print) {
            None => {}
            Some(Diff::Gone) => return Err(format!("{name} was moved or deleted")),
            Some(d) => return Err(d.say(&name, step.print.kind)),
        }
        if step.ident.is_none() || crate::fs::ident_at(&step.at) != step.ident {
            return Err(format!("{name} was replaced by another item with the same name"));
        }
        if !link.shortcut && std::fs::read_link(&step.at).ok().as_deref() != Some(link.target.as_path()) {
            return Err(format!("{name} points somewhere else now"));
        }
        Ok(())
    }

    /// Removes a link Arbor made (the link itself; never what it points at), only while it's
    /// still that object with that target, checked right before through what removes it
    /// ([`remove_if_still`]): a link put at its name since the check, even at the same inode with
    /// the same times and target, is left alone.
    fn remove_link(&self, step: &Step, link: &Link) -> io::Result<()> {
        #[cfg(test)]
        if self.hooks.fail_remove.as_ref().is_some_and(|f| f(&step.at)) {
            return Err(io::Error::new(ErrorKind::PermissionDenied, "permission denied"));
        }
        let want = step.ident.ok_or_else(|| io::Error::other(Replaced))?;
        remove_if_still(&step.at, want, link, &step.print)
    }
}

/// What [`remove_if_still`] says when the item isn't the one it was told to remove.
#[derive(Debug)]
struct Replaced;

impl std::fmt::Display for Replaced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("it was replaced")
    }
}

impl std::error::Error for Replaced {}

/// The undo step for a link just made at `at`: its fingerprint and object identity now.
fn made_step(at: &Path, link: Link) -> io::Result<Step> {
    let m = at.symlink_metadata()?;
    let ident = crate::fs::ident_of(at, &m);
    Ok(Step { print: Print::of(at, &m), at: at.to_path_buf(), back: None, split: None, link: Some(link), ident })
}

/// Unix: removes the link at `at` only while it's `want` pointing at the link's target. The
/// folder is opened once; the identity and target are read relative to it (`statx`,
/// `readlinkat`) and the link removed relative to it (`unlinkat`), so no path is resolved again
/// between the check and the removal.
#[cfg(unix)]
fn remove_if_still(at: &Path, want: crate::fs::Ident, link: &Link, _: &Print) -> io::Result<()> {
    use crate::dirfd;
    let (Some(parent), Some(name)) = (at.parent(), at.file_name()) else { return Err(io::Error::from(ErrorKind::InvalidInput)) };
    let dir = dirfd::open_dir(libc::AT_FDCWD, &crate::ffi::cstr(parent)?, true)?;
    let name = crate::ffi::cstr(name)?;
    if dirfd::ident_at(&dir, &name)? != want {
        return Err(io::Error::other(Replaced));
    }
    if !link.shortcut && dirfd::read_link_at(&dir, &name)? != link.target {
        return Err(io::Error::other(Replaced));
    }
    dirfd::unlink_at(&dir, &name)
}

/// Windows: removes the link at `at` only while it's `want` (with its target, or a shortcut's
/// size and time). The link itself is opened (not what it points at) for deletion, sharing no
/// delete access, so nothing can rename, replace or remove it while it's held; it's checked
/// through that handle and then marked for deletion through it (`FileDispositionInfo`), so
/// nothing is looked up by name again in between.
#[cfg(windows)]
fn remove_if_still(at: &Path, want: crate::fs::Ident, link: &Link, print: &Print) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FileDispositionInfo, SetFileInformationByHandle,
    };
    let f = std::fs::OpenOptions::new()
        .access_mode(DELETE | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(at)?;
    if crate::fs::ident_of_file(&f) != Some(want) {
        return Err(io::Error::other(Replaced));
    }
    if link.shortcut {
        let m = f.metadata()?;
        if m.len() != print.size || m.modified().ok() != print.mtime {
            return Err(io::Error::other(Replaced));
        }
    } else if std::fs::read_link(at)? != link.target {
        // Read by name, but the name can't lead elsewhere while the handle holds it unshared.
        return Err(io::Error::other(Replaced));
    }
    let info = FILE_DISPOSITION_INFO { DeleteFile: 1 };
    // SAFETY: an open handle with DELETE access, and a disposition record as long as we say.
    let ok = unsafe {
        SetFileInformationByHandle(
            f.as_raw_handle(),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// The shell's `IShellLinkW` and `IPersistFile`, by hand (windows-sys has no COM interfaces).
/// Vtable layouts follow `shobjidl_core.h` and `objidl.h`, checked against the `windows`
/// crate's `IShellLinkW_Vtbl` and `IPersistFile_Vtbl`.
#[cfg(windows)]
mod lnk {
    use std::ffi::c_void;
    use std::io;
    use std::path::Path;

    use windows_sys::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };
    use windows_sys::Win32::UI::Shell::ShellLink;
    use windows_sys::core::{GUID, HRESULT};

    use crate::ffi::wide;

    const IID_ISHELLLINKW: GUID = GUID::from_u128(0x000214f9_0000_0000_c000_000000000046);
    const IID_IPERSISTFILE: GUID = GUID::from_u128(0x0000010b_0000_0000_c000_000000000046);

    type P = *mut c_void;
    type W = *const u16;

    // Laid out for COM and read through its pointers, not built here.
    #[allow(dead_code)]
    #[repr(C)]
    struct UnknownVtbl {
        query_interface: unsafe extern "system" fn(P, *const GUID, *mut P) -> HRESULT,
        add_ref: unsafe extern "system" fn(P) -> u32,
        release: unsafe extern "system" fn(P) -> u32,
    }

    // Laid out for COM and read through its pointers, not built here.
    #[allow(dead_code)]
    #[repr(C)]
    struct ShellLinkVtbl {
        base: UnknownVtbl,
        get_path: usize,
        get_id_list: usize,
        set_id_list: usize,
        get_description: usize,
        set_description: usize,
        get_working_directory: usize,
        set_working_directory: unsafe extern "system" fn(P, W) -> HRESULT,
        get_arguments: usize,
        set_arguments: usize,
        get_hotkey: usize,
        set_hotkey: usize,
        get_show_cmd: usize,
        set_show_cmd: usize,
        get_icon_location: usize,
        set_icon_location: usize,
        set_relative_path: usize,
        resolve: usize,
        set_path: unsafe extern "system" fn(P, W) -> HRESULT,
    }

    // Laid out for COM and read through its pointers, not built here.
    #[allow(dead_code)]
    #[repr(C)]
    struct PersistFileVtbl {
        base: UnknownVtbl,
        get_class_id: usize,
        is_dirty: usize,
        load: usize,
        save: unsafe extern "system" fn(P, W, i32) -> HRESULT,
        save_completed: usize,
        get_cur_file: usize,
    }

    fn failed(what: &str, hr: HRESULT) -> io::Error {
        io::Error::other(format!("{what} (code {hr:#010x})"))
    }

    /// Releases a COM object.
    unsafe fn release(p: P) {
        if !p.is_null() {
            // SAFETY: `p` is a live COM object, whose first field points at an IUnknown vtable.
            unsafe { ((**(p as *mut *const UnknownVtbl)).release)(p) };
        }
    }

    /// Writes a shortcut to `target` into `file` (which `IPersistFile::Save` replaces). The
    /// worker thread is its own single-threaded apartment for the call.
    pub fn write(target: &Path, file: &Path) -> io::Result<()> {
        // SAFETY: CoUninitialize balances a successful CoInitializeEx on this thread.
        unsafe {
            let init = CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32);
            if init < 0 {
                return Err(failed("couldn't start the shell", init));
            }
            let r = write_in(target, file);
            CoUninitialize();
            r
        }
    }

    /// SAFETY: COM is initialized on this thread as a single-threaded apartment.
    unsafe fn write_in(target: &Path, file: &Path) -> io::Result<()> {
        let mut link: P = std::ptr::null_mut();
        // SAFETY: valid GUIDs and out-pointer.
        let hr = unsafe { CoCreateInstance(&ShellLink, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID_ISHELLLINKW, &mut link) };
        if hr < 0 || link.is_null() {
            return Err(failed("the shell couldn't make a shortcut", hr));
        }
        // SAFETY: `link` is a live IShellLinkW; the strings are NUL-terminated and outlive the
        // calls; each object is released once, here.
        unsafe {
            let r = save(link, target, file);
            release(link);
            r
        }
    }

    /// SAFETY: `link` is a live IShellLinkW.
    unsafe fn save(link: P, target: &Path, file: &Path) -> io::Result<()> {
        // SAFETY: as the function's contract says.
        unsafe {
            let vt = &**(link as *mut *const ShellLinkVtbl);
            let t = wide(target.as_os_str());
            let hr = (vt.set_path)(link, t.as_ptr());
            if hr < 0 {
                return Err(failed("the shell refused the shortcut's target", hr));
            }
            if let Some(dir) = target.parent() {
                let d = wide(dir.as_os_str());
                (vt.set_working_directory)(link, d.as_ptr());
            }
            let mut pf: P = std::ptr::null_mut();
            let hr = (vt.base.query_interface)(link, &IID_IPERSISTFILE, &mut pf);
            if hr < 0 || pf.is_null() {
                return Err(failed("the shell couldn't save a shortcut", hr));
            }
            let pvt = &**(pf as *mut *const PersistFileVtbl);
            let f = wide(file.as_os_str());
            let hr = (pvt.save)(pf, f.as_ptr(), 1);
            release(pf);
            if hr < 0 { Err(failed("the shell couldn't save the shortcut", hr)) } else { Ok(()) }
        }
    }
}
