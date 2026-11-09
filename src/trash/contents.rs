//! What's in the Trash: emptying it (NORTHSTAR Safety #2: it can't be undone, so it asks
//! first, in Arbor's one dialog), and putting items back where they came from.
//!
//! - **Which trashes.** Linux: the home trash (`$XDG_DATA_HOME/Trash`) and, on every local disk
//!   mounted now ([`crate::mounts::local_points`]), the two the freedesktop.org spec names and
//!   Arbor's own trash uses: `$top/.Trash/$uid` (only if `.Trash` is a sticky folder, not a link)
//!   and `$top/.Trash-$uid`, each a real folder of this user's. macOS: `~/.Trash`, and
//!   `.Trashes/$uid` on each local volume under `/Volumes`. Windows: the shell's Recycle Bin on
//!   every drive (`SHQueryRecycleBinW`, `SHEmptyRecycleBinW`): Windows keeps no folder of it
//!   Arbor could list.
//! - **Counting** ([`Counting`]): when the dialog opens, a thread of its own lists the items
//!   (each with its identity), then adds up their sizes; the dialog shows at once and says the
//!   count when it lands. Closing the dialog stops it. Nothing is kept.
//! - **Emptying** runs on the operation worker (`Op::EmptyTrash`): each item counted is
//!   deleted only while it's still the item that was counted, with the same no-follow,
//!   never-cross-a-mount delete as Delete Permanently (`delete.rs`), then its `.trashinfo`.
//!   An item put in the Trash after the count stays there. Esc stops it between entries.
//! - **macOS privacy.** `~/.Trash` can't be read without Full Disk Access on recent macOS
//!   (`EPERM`); the dialog and the result say how to grant it ([`NEEDS_ACCESS`]).
//! - **Put Back** (Linux): an item's `.trashinfo` says where it came from ([`parse_info`]); it
//!   goes back there, or beside it under a free name when that's taken (`ops`). macOS has no
//!   public API for Finder's Put Back (Finder keeps the original location to itself), and
//!   Windows doesn't let Arbor list the Recycle Bin, so neither offers it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, channel};

use super::BIN;
use crate::fs::Ident;

/// One trash folder: `items` holds what's in it (freedesktop's `files/`, macOS's `.Trash`), and
/// `info` the freedesktop.org `.trashinfo` records (none on macOS).
#[derive(Clone, Debug, PartialEq)]
pub struct Bin {
    pub items: PathBuf,
    pub info: Option<PathBuf>,
}

/// One thing in a Trash, as it was counted: its path, its identity (an item replaced since is
/// never deleted for it), and its record when there is one.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub path: PathBuf,
    pub id: Option<Ident>,
    pub info: Option<PathBuf>,
}

/// Why the Trash couldn't all be read.
#[derive(Clone, Debug, PartialEq)]
pub enum Problem {
    /// macOS refused (`EPERM`): Arbor needs Full Disk Access.
    NeedsAccess,
    /// Anything else, in words.
    Unreadable(String),
}

impl Problem {
    pub fn say(&self) -> String {
        match self {
            Problem::NeedsAccess => NEEDS_ACCESS.into(),
            Problem::Unreadable(why) => why.clone(),
        }
    }
}

/// What macOS needs before Arbor can read `~/.Trash`.
pub const NEEDS_ACCESS: &str = "Arbor needs Full Disk Access to see what's in the Trash: open System Settings → Privacy & Security → Full Disk Access, turn on Arbor (or the app you started it from), then quit and reopen Arbor";

/// The trash folder the sidebar's "Trash" opens: Linux's home trash `files/`, macOS's
/// `~/.Trash`; Windows has none (the Recycle Bin is a shell folder). A path only, no disk I/O.
pub fn place() -> Option<PathBuf> {
    #[cfg(all(unix, not(target_os = "macos")))]
    return Some(super::home_trash().join("files"));
    #[cfg(target_os = "macos")]
    return Some(crate::fs::home().join(".Trash"));
    #[cfg(windows)]
    None
}

/// Emptying uses the shell's Recycle Bin (Windows, outside tests) rather than listing folders.
pub const SHELL: bool = cfg!(all(windows, not(test)));

/// The trash folders Arbor knows about (on the worker: it looks at each disk's top folder).
/// Tests get [`FIXTURE`] instead, so no test ever touches the user's own Trash.
pub fn bins() -> Vec<Bin> {
    #[cfg(test)]
    return FIXTURE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    #[cfg(not(test))]
    platform_bins()
}

/// Tests' trash folders (empty unless a test sets them).
#[cfg(test)]
pub static FIXTURE: std::sync::Mutex<Vec<Bin>> = std::sync::Mutex::new(Vec::new());

#[cfg(all(unix, not(target_os = "macos"), not(test)))]
fn platform_bins() -> Vec<Bin> {
    // SAFETY: getuid can't fail.
    bins_in(&super::home_trash(), &crate::mounts::local_points(), unsafe { libc::getuid() })
}

#[cfg(all(target_os = "macos", not(test)))]
fn platform_bins() -> Vec<Bin> {
    let mut v = vec![Bin { items: crate::fs::home().join(".Trash"), info: None }];
    // SAFETY: getuid can't fail.
    let uid = unsafe { libc::getuid() }.to_string();
    for top in crate::mounts::local_points().into_iter().filter(|t| t.starts_with("/Volumes")) {
        let t = top.join(".Trashes").join(&uid);
        if real_dir(&t, None) {
            v.push(Bin { items: t, info: None });
        }
    }
    v
}

#[cfg(all(windows, not(test)))]
fn platform_bins() -> Vec<Bin> {
    Vec::new()
}

/// Whether `p` is a folder itself (not a link to one), and this user's when `uid` is given.
#[cfg(unix)]
fn real_dir(p: &Path, uid: Option<u32>) -> bool {
    use std::os::unix::fs::MetadataExt;
    p.symlink_metadata().is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink() && uid.is_none_or(|u| m.uid() == u))
}

/// The freedesktop.org trashes: `home` (the home trash) and, on each disk mounted at one of
/// `tops`, `$top/.Trash/$uid` (only under a sticky `.Trash` that isn't a link) and
/// `$top/.Trash-$uid`, each a real folder owned by `uid` with a real `files` folder. A trash
/// reached twice (a bind mount) is listed once.
#[cfg(unix)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn bins_in(home: &Path, tops: &[PathBuf], uid: u32) -> Vec<Bin> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = vec![Bin { items: home.join("files"), info: Some(home.join("info")) }];
    let mut seen: Vec<Ident> = crate::fs::ident_at(&home.join("files")).into_iter().collect();
    for top in tops {
        let shared = top.join(".Trash");
        let admin = shared
            .symlink_metadata()
            .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink() && m.permissions().mode() & 0o1000 != 0)
            .then(|| shared.join(uid.to_string()));
        for root in admin.into_iter().chain([top.join(format!(".Trash-{uid}"))]) {
            let files = root.join("files");
            if !real_dir(&root, Some(uid)) || !real_dir(&files, None) {
                continue;
            }
            let Some(id) = crate::fs::ident_at(&files) else { continue };
            if seen.contains(&id) {
                continue;
            }
            seen.push(id);
            out.push(Bin { items: files, info: Some(root.join("info")) });
        }
    }
    out
}

/// Why folder `dir` couldn't be read: on macOS a refusal (`EPERM`, `EACCES`) means Arbor
/// needs Full Disk Access.
pub fn problem_of(dir: &Path, e: &io::Error, mac: bool) -> Problem {
    if mac && e.kind() == io::ErrorKind::PermissionDenied {
        Problem::NeedsAccess
    } else {
        Problem::Unreadable(format!("Arbor couldn't read “{}” ({e})", crate::fs::pretty(dir)))
    }
}

/// What's in `bins`: each entry of each one's folder, with its identity and record (macOS's
/// `.DS_Store`, Finder's own, isn't an item). A missing folder is an empty trash; the first
/// folder that can't be read is the problem. `None` when `stop` says so.
pub fn list(bins: &[Bin], stop: &dyn Fn() -> bool) -> Option<(Vec<Item>, Option<Problem>)> {
    let (mut items, mut problem) = (Vec::new(), None);
    for b in bins {
        let rd = match std::fs::read_dir(&b.items) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                problem.get_or_insert(problem_of(&b.items, &e, cfg!(target_os = "macos")));
                continue;
            }
        };
        for de in rd {
            if stop() {
                return None;
            }
            let de = match de {
                Ok(de) => de,
                Err(e) => {
                    problem.get_or_insert(problem_of(&b.items, &e, cfg!(target_os = "macos")));
                    break;
                }
            };
            let name = de.file_name();
            if b.info.is_none() && name == ".DS_Store" {
                continue;
            }
            let path = de.path();
            let info = b.info.as_ref().map(|i| {
                let mut n = name.clone();
                n.push(".trashinfo");
                i.join(n)
            });
            items.push(Item { id: crate::fs::ident_at(&path), path, info });
        }
    }
    Some((items, problem))
}

/// The bytes in `items`' files, not following links or entering another disk, and whether all
/// of it could be read. `None` when `stop` says so.
pub fn bytes(items: &[Item], stop: &dyn Fn() -> bool) -> Option<(u64, bool)> {
    fn walk(p: &Path, fence: &crate::mounts::Fence, stop: &dyn Fn() -> bool, n: &mut u64, whole: &mut bool) -> Option<()> {
        if stop() {
            return None;
        }
        let Ok(m) = p.symlink_metadata() else {
            *whole = false;
            return Some(());
        };
        if m.is_dir() && !m.file_type().is_symlink() {
            if fence.crosses(p, &m) {
                return Some(());
            }
            let Ok(rd) = std::fs::read_dir(p) else {
                *whole = false;
                return Some(());
            };
            for de in rd {
                match de {
                    Ok(de) => walk(&de.path(), fence, stop, n, whole)?,
                    Err(_) => *whole = false,
                }
            }
        } else if m.is_file() {
            *n += m.len();
        }
        Some(())
    }
    let mounts = crate::mounts::Mounts::read();
    let (mut n, mut whole) = (0, true);
    for it in items {
        walk(&it.path, &crate::mounts::Fence::new(&it.path, &mounts), stop, &mut n, &mut whole)?;
    }
    Some((n, whole))
}

/// What the count found so far.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    /// The items, once listed (not on Windows, where the shell counts).
    pub items: Option<Vec<Item>>,
    /// How many, once known.
    pub count: Option<u64>,
    /// Their size, once added up, and whether all of it could be read.
    pub bytes: Option<(u64, bool)>,
    pub problem: Option<Problem>,
}

enum Counted {
    Listed(Vec<Item>, Option<Problem>),
    Bytes(u64, bool),
    #[cfg_attr(not(windows), allow(dead_code))]
    Totals(u64, u64),
}

/// The count behind Empty Trash's dialog: one thread, started when the dialog opens, that
/// lists the items, then adds up their sizes, waking the window after each, and ends. Dropping
/// it (the dialog closed) stops it at the next entry.
pub struct Counting {
    rx: Receiver<Counted>,
    stop: Arc<AtomicBool>,
    pub got: Summary,
}

impl Counting {
    pub fn start(waker: Option<rune::Waker>) -> Counting {
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let halt = stop.clone();
        let _ = std::thread::Builder::new().name("arbor-trash-count".into()).spawn(move || {
            let wake = || {
                if let Some(w) = &waker {
                    w();
                }
            };
            let stopped = || halt.load(Relaxed);
            if SHELL {
                #[cfg(windows)]
                if let Some((n, b)) = shell_totals() {
                    let _ = tx.send(Counted::Totals(n, b));
                    wake();
                }
                return;
            }
            let Some((items, problem)) = list(&bins(), &stopped) else { return };
            let sized = items.clone();
            if tx.send(Counted::Listed(items, problem)).is_err() {
                return;
            }
            wake();
            if let Some((n, whole)) = bytes(&sized, &stopped)
                && tx.send(Counted::Bytes(n, whole)).is_ok()
            {
                wake();
            }
        });
        Counting { rx, stop, got: Summary::default() }
    }

    /// Takes what the thread found since. True when anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(m) = self.rx.try_recv() {
            changed = true;
            match m {
                Counted::Listed(items, problem) => {
                    self.got.count = Some(items.len() as u64);
                    self.got.items = Some(items);
                    self.got.problem = problem;
                }
                Counted::Bytes(n, whole) => self.got.bytes = Some((n, whole)),
                Counted::Totals(n, b) => {
                    self.got.count = Some(n);
                    self.got.bytes = Some((b, true));
                }
            }
        }
        changed
    }
}

impl Drop for Counting {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

/// The dialog's words for what the count found: (title, body, button).
pub fn ask_words(got: Option<&Summary>) -> (String, String, &'static str) {
    let title = format!("Empty the {BIN}?");
    let action = if cfg!(windows) { "Empty Recycle Bin" } else { "Empty Trash" };
    let size = match got.and_then(|g| g.bytes) {
        Some((b, true)) => format!(" ({})", crate::fs::fmt_size(b)),
        Some((b, false)) => format!(" (at least {})", crate::fs::fmt_size(b)),
        None => String::new(),
    };
    let body = match got {
        Some(Summary { problem: Some(Problem::NeedsAccess), count: Some(0), .. }) => format!("{NEEDS_ACCESS}."),
        Some(Summary { problem: Some(p), count: Some(n), .. }) if *n > 0 => format!(
            "The {} Arbor can read in the {BIN}{size} will be deleted permanently ({}). This can't be undone.",
            items_word(*n),
            p.say()
        ),
        Some(Summary { problem: Some(p), .. }) => format!("{}.", p.say()),
        Some(Summary { count: Some(0), .. }) => format!("The {BIN} is already empty."),
        Some(Summary { count: Some(1), .. }) => format!("The item in the {BIN}{size} will be deleted permanently. This can't be undone."),
        Some(Summary { count: Some(n), .. }) => {
            format!("The {} in the {BIN}{size} will be deleted permanently. This can't be undone.", items_word(*n))
        }
        _ => format!("Everything in the {BIN} will be deleted permanently. This can't be undone."),
    };
    (title, body, action)
}

fn items_word(n: u64) -> String {
    if n == 1 { "1 item".into() } else { format!("{} items", crate::search::count(n)) }
}

// ---- Windows: the shell's Recycle Bin -----------------------------------------------------

/// How many items the Recycle Bin holds on every drive, and their size.
#[cfg(windows)]
pub fn shell_totals() -> Option<(u64, u64)> {
    use windows_sys::Win32::UI::Shell::{SHQUERYRBINFO, SHQueryRecycleBinW};
    // SAFETY: a zeroed SHQUERYRBINFO with its size set is what the call fills in.
    let mut info: SHQUERYRBINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHQUERYRBINFO>() as u32;
    // SAFETY: a null root means every drive; `info` is valid.
    let hr = unsafe { SHQueryRecycleBinW(std::ptr::null(), &mut info) };
    (hr >= 0).then(|| (info.i64NumItems.max(0) as u64, info.i64Size.max(0) as u64))
}

/// Empties the Recycle Bin of every drive, after Arbor's own dialog asked:
/// `SHERB_NOCONFIRMATION` (Arbor asked already), `SHERB_NOPROGRESSUI` (Arbor's status bar shows
/// it, and the shell's window would be a second dialog), `SHERB_NOSOUND` (Arbor makes no sounds).
/// What's left is counted again afterwards, so the result says what really happened.
#[cfg(windows)]
pub fn shell_empty() -> Result<String, String> {
    use windows_sys::Win32::UI::Shell::{SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI, SHERB_NOSOUND, SHEmptyRecycleBinW};
    let before = shell_totals();
    if before.is_some_and(|(n, _)| n == 0) {
        return Ok(format!("The {BIN} is already empty"));
    }
    // SAFETY: no window, a null root (every drive), and documented flags.
    let hr =
        unsafe { SHEmptyRecycleBinW(std::ptr::null_mut(), std::ptr::null(), SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND) };
    shell_note(before, hr, shell_totals())
}

/// The result of emptying the Recycle Bin, from its totals `before` and `after` and what the
/// call returned.
#[cfg(any(windows, test))]
pub fn shell_note(before: Option<(u64, u64)>, hr: i32, after: Option<(u64, u64)>) -> Result<String, String> {
    match (before, after) {
        (Some((n, b)), Some((0, _))) => {
            Ok(format!("Emptied the {BIN}: deleted {} ({}) permanently", items_word(n), crate::fs::fmt_size(b)))
        }
        (_, Some((0, _))) => Ok(format!("Emptied the {BIN}")),
        (Some((n, _)), Some((left, _))) if left < n => {
            Err(format!("Emptied part of the {BIN}: deleted {}, {} still in it (code {hr:#010x})", items_word(n - left), items_word(left)))
        }
        (_, Some((left, _))) => Err(format!("Couldn't empty the {BIN}: {} still in it (code {hr:#010x})", items_word(left))),
        (_, None) if hr >= 0 => Ok(format!("Emptied the {BIN}")),
        (_, None) => Err(format!("Couldn't empty the {BIN} (code {hr:#010x})")),
    }
}

// ---- Put Back (freedesktop.org) -------------------------------------------------------------

/// A freedesktop.org trash, from its `files` folder: the trash's own folder (with `info/`) and,
/// for a trash on another disk, that disk's top folder, which relative paths in its records
/// start from.
#[derive(Clone, Debug, PartialEq)]
pub struct TrashDir {
    pub root: PathBuf,
    pub top: Option<PathBuf>,
}

/// Which trash `files` (a folder's path) is the `files/` folder of: the home trash `home`, or a
/// disk's `$top/.Trash-$uid` or `$top/.Trash/$uid`. By its path alone, no disk I/O.
pub fn trash_of(files: &Path, home: Option<&Path>) -> Option<TrashDir> {
    if files.file_name()? != "files" {
        return None;
    }
    let root = files.parent()?;
    if home == Some(root) {
        return Some(TrashDir { root: root.into(), top: None });
    }
    let name = root.file_name()?.to_string_lossy();
    let up = root.parent()?;
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if name.strip_prefix(".Trash-").is_some_and(numeric) {
        return Some(TrashDir { root: root.into(), top: Some(up.into()) });
    }
    if up.file_name()? == ".Trash" && numeric(&name) {
        return Some(TrashDir { root: root.into(), top: Some(up.parent()?.into()) });
    }
    None
}

/// Whether folder `dir` holds trashed items Put Back can restore (Linux only).
pub fn can_put_back_in(dir: &Path) -> bool {
    #[cfg(all(unix, not(target_os = "macos")))]
    return trash_of(dir, Some(&super::home_trash())).is_some();
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        let _ = dir;
        false
    }
}

/// Where an item came from, from its `.trashinfo` text: the `Path=` key of its `[Trash Info]`
/// group, URL-decoded; a relative one is from `top` (a trash on another disk). A path with `..`
/// in it, or none, is refused rather than guessed.
pub fn parse_info(text: &str, top: Option<&Path>) -> Result<PathBuf, String> {
    let mut in_group = false;
    let mut found = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with('[') {
            in_group = line == "[Trash Info]";
            continue;
        }
        if in_group && let Some(v) = line.strip_prefix("Path=") {
            found = Some(v);
            break;
        }
    }
    let raw = found.ok_or("its record in the Trash doesn't say where it came from")?;
    let path = PathBuf::from(os_from_bytes(url_decode(raw)));
    if path.as_os_str().is_empty() || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err("its record in the Trash names a path Arbor won't trust".into());
    }
    if path.is_absolute() {
        return Ok(path);
    }
    match top {
        Some(top) => Ok(top.join(path)),
        None => Err("its record in the Trash names a relative path".into()),
    }
}

/// Where the trashed item at `trashed` (in a trash's `files/`) came from, and its record's path.
pub fn original(trashed: &Path, home: Option<&Path>) -> Result<(PathBuf, PathBuf), String> {
    let t = trashed.parent().and_then(|f| trash_of(f, home)).ok_or("it isn't in a Trash Arbor knows")?;
    let mut name = trashed.file_name().ok_or("it has no name")?.to_os_string();
    name.push(".trashinfo");
    let info = t.root.join("info").join(name);
    let text = read_info(&info)?;
    Ok((parse_info(&text, t.top.as_deref())?, info))
}

/// A `.trashinfo` file's text: a regular file (not a link) of at most 64 KB.
fn read_info(info: &Path) -> Result<String, String> {
    use std::io::Read;
    match info.symlink_metadata() {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err("it has no record in the Trash saying where it came from".into()),
        Err(e) => return Err(format!("its record in the Trash couldn't be read ({e})")),
        Ok(m) if !m.is_file() => return Err("its record in the Trash isn't a file".into()),
        Ok(_) => {}
    }
    let mut s = String::new();
    std::fs::File::open(info)
        .and_then(|f| f.take(64 * 1024).read_to_string(&mut s))
        .map_err(|e| format!("its record in the Trash couldn't be read ({e})"))?;
    Ok(s)
}

/// `%XX` escapes back to their bytes.
fn url_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && let (Some(h), Some(l)) = (b.get(i + 1).and_then(|&c| hex(c)), b.get(i + 2).and_then(|&c| hex(c)))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

fn os_from_bytes(b: Vec<u8>) -> std::ffi::OsString {
    #[cfg(unix)]
    return <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(b);
    #[cfg(not(unix))]
    std::ffi::OsString::from(String::from_utf8_lossy(&b).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-contents-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// The home trash, then each disk's `.Trash/$uid` (only under a sticky `.Trash` that isn't
    /// a link) and `.Trash-$uid` (a real folder, not a link); one reached twice is listed once.
    #[cfg(unix)]
    #[test]
    fn bins_are_the_spec_trashes_and_never_an_unsafe_one() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("bins");
        // SAFETY: getuid can't fail.
        let uid = unsafe { libc::getuid() };
        let home = d.join("home/Trash");
        fs::create_dir_all(home.join("files")).unwrap();
        // A sticky .Trash with this user's folder, and a .Trash-uid beside it.
        let a = d.join("a");
        fs::create_dir_all(a.join(format!(".Trash/{uid}/files"))).unwrap();
        fs::set_permissions(a.join(".Trash"), fs::Permissions::from_mode(0o1777)).unwrap();
        fs::create_dir_all(a.join(format!(".Trash-{uid}/files"))).unwrap();
        // A .Trash that isn't sticky: not safe to share, so not used.
        let b = d.join("b");
        fs::create_dir_all(b.join(format!(".Trash/{uid}/files"))).unwrap();
        fs::set_permissions(b.join(".Trash"), fs::Permissions::from_mode(0o777)).unwrap();
        // A .Trash-uid that's a link to somewhere else: never followed.
        let c = d.join("c");
        fs::create_dir_all(c.join("elsewhere/files")).unwrap();
        std::os::unix::fs::symlink(c.join("elsewhere"), c.join(format!(".Trash-{uid}"))).unwrap();
        let got = bins_in(&home, &[a.clone(), b, c, a.clone()], uid);
        let items: Vec<PathBuf> = got.iter().map(|b| b.items.clone()).collect();
        assert_eq!(items, [home.join("files"), a.join(format!(".Trash/{uid}/files")), a.join(format!(".Trash-{uid}/files"))]);
        assert_eq!(got[1].info, Some(a.join(format!(".Trash/{uid}/info"))));
        let _ = fs::remove_dir_all(&d);
    }

    /// Each entry is an item with its identity and record; a missing trash is an empty one;
    /// macOS's `.DS_Store` (a trash with no records) isn't an item.
    #[test]
    fn list_finds_items_and_their_records() {
        let d = tmp("list");
        fs::create_dir_all(d.join("t/files/folder")).unwrap();
        fs::write(d.join("t/files/a.txt"), "a").unwrap();
        fs::create_dir_all(d.join("mac")).unwrap();
        fs::write(d.join("mac/.DS_Store"), "x").unwrap();
        fs::write(d.join("mac/b"), "b").unwrap();
        let bins = [
            Bin { items: d.join("t/files"), info: Some(d.join("t/info")) },
            Bin { items: d.join("mac"), info: None },
            Bin { items: d.join("missing/files"), info: Some(d.join("missing/info")) },
        ];
        let (mut items, problem) = list(&bins, &|| false).unwrap();
        items.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(problem, None);
        let paths: Vec<&Path> = items.iter().map(|i| i.path.as_path()).collect();
        assert_eq!(paths, [d.join("mac/b"), d.join("t/files/a.txt"), d.join("t/files/folder")]);
        assert_eq!(items[1].info, Some(d.join("t/info/a.txt.trashinfo")));
        assert_eq!(items[0].info, None);
        assert!(items.iter().all(|i| i.id.is_some()));
        assert!(list(&bins, &|| true).is_none(), "a stop stops it");
        let _ = fs::remove_dir_all(&d);
    }

    /// The size counts what's in the items, never what a link inside one points to.
    #[test]
    fn bytes_never_follow_a_link() {
        let d = tmp("bytes");
        fs::create_dir_all(d.join("files/f")).unwrap();
        fs::write(d.join("files/f/x"), vec![0u8; 1000]).unwrap();
        fs::write(d.join("files/y"), vec![0u8; 24]).unwrap();
        fs::write(d.join("big"), vec![0u8; 100_000]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(d.join("big"), d.join("files/f/link")).unwrap();
        let (items, _) = list(&[Bin { items: d.join("files"), info: None }], &|| false).unwrap();
        assert_eq!(bytes(&items, &|| false), Some((1024, true)));
        assert_eq!(bytes(&items, &|| true), None);
        let _ = fs::remove_dir_all(&d);
    }

    /// The dialog says the count and size when known, an empty Trash plainly, and on macOS how
    /// to grant Full Disk Access when the Trash can't be read.
    #[test]
    fn the_dialog_says_what_the_count_found() {
        let (title, body, action) = ask_words(None);
        assert_eq!(title, format!("Empty the {BIN}?"));
        assert_eq!(body, format!("Everything in the {BIN} will be deleted permanently. This can't be undone."));
        assert!(action.starts_with("Empty"));
        let got = |count, bytes, problem| Summary { items: None, count, bytes, problem };
        let say = |g: Summary| ask_words(Some(&g)).1;
        assert_eq!(
            say(got(Some(1234), None, None)),
            format!("The 1,234 items in the {BIN} will be deleted permanently. This can't be undone.")
        );
        assert_eq!(
            say(got(Some(3), Some((2048, true)), None)),
            format!("The 3 items in the {BIN} (2.0 KB) will be deleted permanently. This can't be undone.")
        );
        assert!(say(got(Some(1), Some((10, false)), None)).starts_with(&format!("The item in the {BIN} (at least")));
        assert_eq!(say(got(Some(0), None, None)), format!("The {BIN} is already empty."));
        assert_eq!(say(got(Some(0), None, Some(Problem::NeedsAccess))), format!("{NEEDS_ACCESS}."));
        let partly = say(got(Some(2), None, Some(Problem::Unreadable("Arbor couldn't read “/x” (denied)".into()))));
        assert!(partly.contains("The 2 items Arbor can read") && partly.ends_with("This can't be undone."), "{partly}");
    }

    #[test]
    fn a_refusal_on_macos_means_full_disk_access() {
        let eperm = io::Error::from_raw_os_error(1);
        assert_eq!(problem_of(Path::new("/x/.Trash"), &eperm, true), Problem::NeedsAccess);
        assert!(matches!(problem_of(Path::new("/x/.Trash"), &eperm, false), Problem::Unreadable(_)));
        let gone = io::Error::from(io::ErrorKind::Other);
        assert!(matches!(problem_of(Path::new("/x"), &gone, true), Problem::Unreadable(_)));
    }

    /// Windows: the result counts the Recycle Bin again afterwards and says what's still there.
    #[test]
    fn the_recycle_bins_result_is_counted_again() {
        assert!(shell_note(Some((3, 2048)), 0, Some((0, 0))).unwrap().contains("deleted 3 items (2.0 KB) permanently"));
        assert!(shell_note(None, 0, Some((0, 0))).unwrap().starts_with("Emptied"));
        let part = shell_note(Some((5, 1)), 0x8000_4005u32 as i32, Some((2, 1))).unwrap_err();
        assert!(part.contains("deleted 3 items, 2 items still in it"), "{part}");
        assert!(shell_note(None, -1, None).is_err());
        assert!(shell_note(Some((1, 1)), 0, Some((1, 1))).unwrap_err().contains("1 item still in it"));
    }

    /// A record's `Path=` is read from its group, URL-decoded, and a relative one is from the
    /// disk's top folder; what can't be trusted is refused, not guessed.
    #[test]
    fn a_record_says_where_an_item_came_from() {
        let top = Path::new("/media/usb");
        let info = |p: &str| format!("[Trash Info]\nPath={p}\nDeletionDate=2026-11-09T10:00:00\n");
        assert_eq!(parse_info(&info("/home/u/My%20Docs/a.txt"), None).unwrap(), Path::new("/home/u/My Docs/a.txt"));
        assert_eq!(parse_info(&info("docs/b%25.txt"), Some(top)).unwrap(), top.join("docs/b%.txt"));
        assert!(parse_info(&info("docs/b.txt"), None).is_err(), "relative, and no top folder to start from");
        assert!(parse_info(&info("/home/u/../../etc/passwd"), None).is_err());
        assert!(parse_info("[Other]\nPath=/a\n", None).is_err(), "only the Trash Info group counts");
        assert!(parse_info("[Trash Info]\nDeletionDate=x\n", None).is_err());
        assert_eq!(parse_info("[Other]\nPath=/x\n[Trash Info]\r\nPath=/y%2\r\n", None).unwrap(), Path::new("/y%2"));
    }

    #[test]
    fn trashes_are_known_by_their_paths() {
        let home = Path::new("/home/u/.local/share/Trash");
        assert_eq!(trash_of(&home.join("files"), Some(home)), Some(TrashDir { root: home.into(), top: None }));
        let usb = Path::new("/media/usb");
        assert_eq!(trash_of(&usb.join(".Trash-1000/files"), Some(home)).unwrap().top.as_deref(), Some(usb));
        assert_eq!(trash_of(&usb.join(".Trash/1000/files"), Some(home)).unwrap().top.as_deref(), Some(usb));
        assert_eq!(trash_of(&usb.join(".Trash-1000/info"), Some(home)), None);
        assert_eq!(trash_of(&usb.join(".Trash-x/files"), Some(home)), None);
        assert_eq!(trash_of(Path::new("/home/u/files"), Some(home)), None);
    }
}
