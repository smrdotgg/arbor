//! Archives: Compress makes a `.zip`, and Extract Here unpacks `.zip`, `.tar`, `.tar.gz` and
//! `.tgz`. This module reads and writes the formats (`zip.rs`, `tar.rs`), decides what an
//! archive's entries become on disk ([`plan`]) and writes them there without following any link
//! (`out.rs`). The operation worker drives both (`ops/archive.rs`): progress, cancel, cleanup,
//! undo.
//!
//! Extracting reads the whole list of entries first and refuses the archive before writing
//! anything when one entry would land outside the folder it makes: an absolute path, a `..`,
//! a drive letter, a UNC path, a NUL byte, or a link whose target leaves the folder (or goes
//! through another link the archive makes, which could). Names that aren't valid here are
//! changed (batch rename's rules, [`crate::rename::invalid`]) and said so. Sizes are bounded
//! against decompression bombs ([`limits`]): the total and the number of entries are checked
//! up front, and every entry is cut off at the size it declared. Files, folders and links are
//! created relative to an open folder with `O_NOFOLLOW` and `O_EXCL`, so nothing the archive
//! made (or anyone else put there) is ever followed or overwritten; links are made last.
//!
//! No crate: the ZIP subset Arbor needs (stored and deflate, ZIP64, UTF-8 names, data
//! descriptors on read) and a tar reader are small, over `flate2` and `crc32fast`, which the
//! lock file already has through `png` (DECISIONS.md, 2026-11-11).

use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::rename::{Platform, invalid};

pub mod out;
pub mod tar;
#[cfg(test)]
pub(crate) mod tests;
pub mod zip;

/// An archive format Extract Here reads, told by the name's extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Zip,
    Tar,
    TarGz,
}

impl Format {
    /// The format of a file named `name`, from its extension (any case).
    /// Allocates nothing: the palette asks it for every selected item.
    pub fn of(name: &str) -> Option<Format> {
        let ends = |ext: &str| name.len() > ext.len() && name.as_bytes()[name.len() - ext.len()..].eq_ignore_ascii_case(ext.as_bytes());
        if ends(".zip") {
            Some(Format::Zip)
        } else if ends(".tar.gz") || ends(".tgz") {
            Some(Format::TarGz)
        } else if ends(".tar") {
            Some(Format::Tar)
        } else {
            None
        }
    }

    /// `name` without its archive extension ("Photos.zip" → "Photos", "a.tar.gz" → "a"), or
    /// "Archive" when nothing would be left.
    pub fn stem(name: &str) -> String {
        let l = name.to_ascii_lowercase();
        let cut = [".tar.gz", ".tgz", ".tar", ".zip"].iter().find(|e| l.ends_with(*e)).map_or(0, |e| e.len());
        let s = name[..name.len() - cut].trim();
        if s.is_empty() || s == "." || s == ".." { "Archive".into() } else { s.into() }
    }
}

/// Whether a file named `name` is an archive Extract Here reads.
pub fn is_archive(name: &str) -> bool {
    Format::of(name).is_some()
}

/// The bounds against decompression bombs, checked on the list before anything is written. No
/// ratio of contents to archive size: real archives pass any useful one (logs at 294 times,
/// zeros at 1,000, a sparse disk image far more). What declares sizes is held to them while
/// it's written (each entry is cut off at its size, and must reach it with its CRC), entries
/// may not share data (`zip.rs`), the disk must have room ([`room`]), and Esc stops it.
pub mod limits {
    /// Entries in one archive (each is a file, folder or link to create).
    pub const ENTRIES: usize = 1_000_000;
    /// Total size of what an archive unpacks to.
    pub const TOTAL: u64 = 1 << 40;
    /// What's left free on the disk after extracting: at least this, and [`SPARE_PART`] of
    /// what was free.
    pub const SPARE: u64 = 1 << 30;
    pub const SPARE_PART: u64 = 20;
    /// Folders inside folders, and bytes in an entry's path.
    pub const DEPTH: usize = 256;
    pub const PATH: usize = 16 * 1024;
    /// A link's target, a tar's long name or extended header.
    pub const META: usize = 1 << 20;
}

/// Why an archive can't be extracted or made: what the status bar says after its name.
#[derive(Debug)]
pub enum Error {
    /// Asked to stop.
    Cancelled,
    /// Damaged or not an archive of its format (truncated, bad CRC, impossible sizes).
    Corrupt(String),
    /// Refused for safety: an entry would land outside the folder, or a bomb.
    Refused(String),
    /// A feature Arbor doesn't read (encryption, another compression method).
    Unsupported(String),
    Io(io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Cancelled => f.write_str("cancelled"),
            Error::Corrupt(s) => write!(f, "it's damaged ({s})"),
            Error::Refused(s) | Error::Unsupported(s) => f.write_str(s),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        match e.kind() {
            io::ErrorKind::UnexpectedEof => Error::Corrupt("it ends too soon".into()),
            // flate2 says a deflate stream is broken this way.
            io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => Error::Corrupt(e.to_string()),
            _ => Error::Io(e),
        }
    }
}

/// What an entry is, as the archive says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    File {
        size: u64,
    },
    Dir,
    /// A symbolic link, and its target as stored.
    Link(String),
    /// Something Arbor doesn't make (a hard link, a device, a FIFO): left out, with this note.
    Skip(&'static str),
}

/// One entry of an archive's list: its name (`/`-separated as stored, decoded), what it is,
/// its Unix mode bits when the archive has them, and its modification time.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    pub mode: Option<u32>,
    pub mtime: Option<SystemTime>,
    /// The name wasn't valid UTF-8 (a tar's raw bytes): shown with replacement characters.
    pub lossy: bool,
}

/// Where an entry goes, below the folder Extract makes.
#[derive(Debug, Clone)]
pub struct Planned {
    /// Its index in the archive's list.
    pub index: usize,
    /// Its path below the folder, each part valid here.
    pub rel: Vec<String>,
    pub kind: Kind,
    pub mode: Option<u32>,
    pub mtime: Option<SystemTime>,
}

/// What an archive becomes: the folder made next to it (named `root`; its own time when the
/// archive has an entry for it) and the entries inside, in archive order.
#[derive(Debug)]
pub struct Plan {
    pub root: String,
    pub root_mtime: Option<SystemTime>,
    pub items: Vec<Planned>,
    /// Bytes of files to write.
    pub total: u64,
    /// Notes for the status bar: names changed, entries left out.
    pub notes: Vec<String>,
}

/// The parts of entry name `name`, refusing one that would land outside the folder it's
/// extracted into. `\` separates too (archives made on Windows), so `a\..\..\x` is a `..`
/// everywhere. Empty and `.` parts are dropped; nothing left means nothing to make.
pub fn parts(name: &str) -> Result<Vec<&str>, String> {
    let shown = || name.chars().take(80).collect::<String>();
    if name.contains('\0') {
        return Err(format!("“{}” has a NUL character in its name", shown()));
    }
    if name.starts_with(['/', '\\']) {
        return Err(format!("“{}” is an absolute path", shown()));
    }
    let b = name.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return Err(format!("“{}” starts with a drive letter", shown()));
    }
    if name.len() > limits::PATH {
        return Err(format!("“{}…” has a path longer than {} bytes", shown(), limits::PATH));
    }
    let mut v = Vec::new();
    for p in name.split(['/', '\\']) {
        match p {
            "" | "." => {}
            ".." => return Err(format!("“{}” goes up out of the folder (“..”)", shown())),
            p => v.push(p),
        }
    }
    if v.len() > limits::DEPTH {
        return Err(format!("“{}…” is more than {} folders deep", shown(), limits::DEPTH));
    }
    Ok(v)
}

/// `part` made valid as a name on `p`, or `None` when it already is: characters the platform
/// doesn't allow become `_`, a reserved device name gets `_` in front, a trailing dot or space
/// (Windows) becomes `_`, and a name that's too long is cut, keeping its extension.
pub fn fix(part: &str, p: Platform) -> Option<String> {
    invalid(part, p)?;
    let mut s: String =
        part.chars().map(|c| if c == '/' || c == '\0' || (p == Platform::Windows && bad_on_windows(c)) { '_' } else { c }).collect();
    if p == Platform::Windows {
        let kept = s.trim_end_matches(['.', ' ']).len();
        if kept < s.len() {
            s.truncate(kept);
            s.push('_');
        }
        if invalid(&s, p).is_some_and(|why| why.contains("reserved")) {
            s.insert(0, '_');
        }
    }
    let units = |s: &str| if p == Platform::Windows { s.encode_utf16().count() } else { s.len() };
    if units(&s) > crate::rename::MAX_NAME {
        let (stem, ext) = match s.rfind('.') {
            Some(i) if i > 0 && s.len() - i <= 16 => (s[..i].to_string(), s[i..].to_string()),
            _ => (s.clone(), String::new()),
        };
        let mut stem = stem;
        while units(&stem) + units(&ext) > crate::rename::MAX_NAME {
            stem.pop();
        }
        s = stem + &ext;
    }
    if invalid(&s, p).is_some() { Some("_".into()) } else { Some(s) }
}

fn bad_on_windows(c: char) -> bool {
    matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*') || (c as u32) < 32
}

/// What `entries` (an archive's list, from a file named `archive`)
/// become when extracted on `p`, or why the archive is refused (an entry outside the folder, a
/// link leaving it, too large: nothing is written then).
///
/// The folder is named after the archive, unless everything in it is inside one folder: then
/// that folder is what's made (so "Photos.zip" holding "Photos/…" makes "Photos", not
/// "Photos/Photos").
pub fn plan(archive: &str, entries: &[Entry], p: Platform) -> Result<Plan, Error> {
    if entries.len() > limits::ENTRIES {
        return Err(Error::Refused(format!("it has more than {} entries", group(limits::ENTRIES as u64))));
    }
    let mut notes = Vec::new();
    let (mut renamed, mut lossy, mut first_renamed) = (0usize, 0usize, None);
    let mut skipped: Vec<(&'static str, usize, String)> = Vec::new();
    // Each entry's parts, valid here.
    let mut items: Vec<Planned> = Vec::with_capacity(entries.len());
    for (index, e) in entries.iter().enumerate() {
        let raw = parts(&e.name).map_err(Error::Refused)?;
        if let Kind::Skip(why) = e.kind {
            match skipped.iter_mut().find(|s| s.0 == why) {
                Some(s) => s.1 += 1,
                None => skipped.push((why, 1, e.name.clone())),
            }
            continue;
        }
        if raw.is_empty() {
            continue; // "./" or "/": the folder itself
        }
        // macOS's resource forks and extended attributes, which its Archive Utility folds back
        // into the files: nothing a user put there.
        if raw[0] == "__MACOSX" {
            continue;
        }
        if e.lossy {
            lossy += 1;
        }
        let mut rel = Vec::with_capacity(raw.len());
        for part in raw {
            match fix(part, p) {
                Some(fixed) => {
                    renamed += 1;
                    first_renamed.get_or_insert_with(|| (part.to_string(), fixed.clone()));
                    rel.push(fixed);
                }
                None => rel.push(part.to_string()),
            }
        }
        items.push(Planned { index, rel, kind: e.kind.clone(), mode: e.mode, mtime: e.mtime });
    }
    let total = items.iter().map(|i| if let Kind::File { size } = i.kind { size } else { 0 }).fold(0u64, u64::saturating_add);
    check_size(total)?;
    // One folder holding everything: it's the folder made.
    let top = items.first().map(|i| i.rel[0].clone());
    let single = top.as_ref().is_some_and(|t| items.iter().all(|i| &i.rel[0] == t && (i.rel.len() > 1 || i.kind == Kind::Dir)));
    let (root, mut root_mtime) = match (single, top) {
        (true, Some(t)) => (t, None),
        _ => (fix(&Format::stem(archive), p).unwrap_or_else(|| Format::stem(archive)), None),
    };
    if single {
        root_mtime = items.iter().find(|i| i.rel.len() == 1).and_then(|i| i.mtime);
        items.retain(|i| i.rel.len() > 1);
        for i in &mut items {
            i.rel.remove(0);
        }
    }
    // After the folder is chosen: a link's target is checked against the folder made, so
    // "Photos/l → ../x" in a "Photos.zip" made as "Photos" is outside.
    unsafe_links(&mut items, &mut notes);
    // Windows makes links only for administrators or in Developer Mode: they're left out there.
    if p == Platform::Windows
        && let Some(first) = items.iter().find(|i| matches!(i.kind, Kind::Link(_))).map(|i| i.rel.join("/"))
    {
        let n = items.iter().filter(|i| matches!(i.kind, Kind::Link(_))).count();
        items.retain(|i| !matches!(i.kind, Kind::Link(_)));
        let more = if n > 1 { format!(" (and {} more)", n - 1) } else { String::new() };
        notes.push(format!("“{first}” was left out: links aren't extracted on Windows{more}"));
    }
    if renamed > 0 {
        let (from, to) = first_renamed.unwrap_or_default();
        let more = if renamed > 1 { format!(" (and {} more)", renamed - 1) } else { String::new() };
        notes.push(format!("“{from}” was renamed “{to}”, a name that's valid here{more}"));
    }
    if lossy > 0 {
        notes.push(format!("{lossy} name{} weren't UTF-8 and got replacement characters", if lossy == 1 { "" } else { "s" }));
    }
    for (why, n, first) in skipped {
        let more = if n > 1 { format!(" (and {} more)", n - 1) } else { String::new() };
        notes.push(format!("“{first}” was left out: {why}{more}"));
    }
    Ok(Plan { root, root_mtime, items, total, notes })
}

/// Refuses `total` bytes of contents past [`limits::TOTAL`].
pub fn check_size(total: u64) -> Result<(), Error> {
    use crate::fs::fmt_size;
    if total > limits::TOTAL {
        return Err(Error::Refused(format!(
            "it's too large to extract: it would unpack to {}, more than Arbor extracts ({})",
            fmt_size(total),
            fmt_size(limits::TOTAL)
        )));
    }
    Ok(())
}

/// Whether `total` bytes fit on a disk with `free` bytes free, leaving [`limits::SPARE`] or
/// [`limits::SPARE_PART`] of it, whichever is more; why not, in words.
pub fn room(total: u64, free: u64) -> Result<(), String> {
    use crate::fs::fmt_size;
    let spare = limits::SPARE.max(free / limits::SPARE_PART);
    if total > free.saturating_sub(spare) {
        return Err(format!(
            "it's too large to extract: it unpacks to {}, and {} is free on the disk (Arbor leaves at least {} free)",
            fmt_size(total),
            fmt_size(free),
            fmt_size(spare)
        ));
    }
    Ok(())
}

/// Leaves out every link whose target could lead out of the folder ([`link_target`]), with a
/// note naming them; the rest of the archive is extracted. (An entry whose own path leads out
/// refuses the whole archive: see [`parts`].)
fn unsafe_links(items: &mut Vec<Planned>, notes: &mut Vec<String>) {
    let mut out = Vec::new();
    items.retain(|i| {
        let Kind::Link(target) = &i.kind else { return true };
        let ok = link_target(i.rel.len() - 1, target).is_ok();
        if !ok {
            out.push(format!("“{}” → “{}”", i.rel.join("/"), target.chars().take(80).collect::<String>()));
        }
        ok
    });
    if !out.is_empty() {
        let n = out.len();
        let shown = out.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
        let more = if n > 3 { format!(" (and {} more)", n - 3) } else { String::new() };
        let what = if n == 1 { "a link that points" } else { "links that point" };
        notes.push(format!("Left out {what} outside the folder, or down and back up: {shown}{more}"));
    }
}

/// Whether a link `depth` folders down inside the folder made may have `target`, by the target
/// alone, never by the names of other entries (which can be renamed when they're made, or
/// spelled differently by the disk): it must be relative, its `..` parts may only come first,
/// and there may be no more of them than `depth`. Empty and `.` parts are nothing. Such a
/// target leads to the link's own folder or one of the folders holding it, then down by name,
/// so it stays inside even through another link of the archive, which keeps to the same rule
/// (the folders above a link are real folders: `out.rs` makes them without following
/// anything, and makes links last).
pub fn link_target(depth: usize, target: &str) -> Result<(), String> {
    if target.is_empty() || target.contains('\0') {
        return Err("has no usable target".into());
    }
    if target.len() > limits::META {
        return Err("has a target too long to be real".into());
    }
    let shown = || target.chars().take(80).collect::<String>();
    let b = target.as_bytes();
    if target.starts_with(['/', '\\']) || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') {
        return Err(format!("points outside the folder (to “{}”)", shown()));
    }
    let (mut up, mut down) = (0, false);
    for part in target.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." if down => return Err(format!("goes down and back up (“{}”), which Arbor doesn't follow", shown())),
            ".." => up += 1,
            _ => down = true,
        }
    }
    if up > depth {
        return Err(format!("points outside the folder (to “{}”)", shown()));
    }
    Ok(())
}

/// 1234567 as "1,234,567".
pub fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Free bytes for this user on the disk of folder `dir`, when the OS says.
pub fn free_space(dir: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        let c = crate::ffi::cstr(dir).ok()?;
        // SAFETY: a NUL-terminated path and a zeroed out-buffer.
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
        Some((s.f_bavail as u64).saturating_mul(s.f_frsize as u64))
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let w = crate::ffi::wide(dir);
        let mut free = 0u64;
        // SAFETY: a NUL-terminated wide path and an out-pointer to a u64.
        let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) } != 0;
        ok.then_some(free)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = dir;
        None
    }
}

// ---- times ----------------------------------------------------------------------------------

/// Days since 1970-01-01 of a civil date (proleptic Gregorian; Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of a day since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Seconds east of UTC in this computer's time zone at Unix time `t`.
fn utc_offset(t: i64) -> i64 {
    #[cfg(unix)]
    {
        let tt = t as libc::time_t;
        // SAFETY: a zeroed tm is a valid out-buffer; localtime_r is thread-safe.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { libc::localtime_r(&tt, &mut tm) }.is_null() {
            return 0;
        }
        #[allow(clippy::unnecessary_cast)]
        {
            tm.tm_gmtoff as i64
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
        let ft = (t + 11_644_473_600) * 10_000_000;
        let utc = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
        let mut local = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        // SAFETY: two valid FILETIMEs.
        if unsafe { FileTimeToLocalFileTime(&utc, &mut local) } == 0 {
            return 0;
        }
        let l = (i64::from(local.dwHighDateTime) << 32) | i64::from(local.dwLowDateTime);
        (l - ft) / 10_000_000
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = t;
        0
    }
}

/// An MS-DOS date and time (local time, two-second steps, 1980 to 2107), as ZIP stores them.
pub fn to_dos(t: SystemTime) -> (u16, u16) {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let local = secs + utc_offset(secs);
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    if !(1980..=2107).contains(&y) {
        return if y < 1980 { (0x21, 0) } else { (0xFF9F, 0xBF7D) };
    }
    let s = local.rem_euclid(86_400);
    let date = ((y - 1980) as u16) << 9 | (m as u16) << 5 | d as u16;
    let time = ((s / 3600) as u16) << 11 | (((s % 3600) / 60) as u16) << 5 | ((s % 60) / 2) as u16;
    (date, time)
}

/// An MS-DOS date and time (local time) as a time; `None` when it isn't a valid one.
pub fn from_dos(date: u16, time: u16) -> Option<SystemTime> {
    let (y, m, d) = (1980 + i64::from(date >> 9), u32::from((date >> 5) & 15), u32::from(date & 31));
    let (h, mi, s) = (i64::from(time >> 11), i64::from((time >> 5) & 63), i64::from(time & 31) * 2);
    if !(1..=12).contains(&m) || d == 0 || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    let local = days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 + s;
    let guess = local - utc_offset(local);
    from_unix(local - utc_offset(guess))
}

/// Unix seconds as a time (before 1970 too).
pub fn from_unix(secs: i64) -> Option<SystemTime> {
    if secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))
    }
}

/// A time as Unix seconds.
pub fn to_unix(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// Names in ZIPs without the UTF-8 flag: code page 437 (the spec's), unless the bytes are valid
/// UTF-8, as many tools write without saying.
pub fn decode_cp437(b: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(b) {
        return s.into();
    }
    const HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}";
    let high: Vec<char> = HIGH.chars().collect();
    b.iter().map(|&c| if c < 0x80 { c as char } else { high[usize::from(c - 0x80)] }).collect()
}
