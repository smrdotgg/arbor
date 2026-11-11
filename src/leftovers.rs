//! What a file operation cut short by quitting left on disk, kept until the next launch says so.
//!
//! Closing the last window (or quitting) with an operation running cancels it at once and lets
//! the window go; the operation's own cleanup (a cancelled copy removing what it made) carries
//! on, off the UI thread, for at most [`QUIT_WAIT`]. Nothing it couldn't finish is lost:
//!
//! 1. Right after the close, the quit thread writes a provisional record naming what's only
//!    partly there (the copy being made, and anything cleanup already had to leave). The close
//!    callback itself writes nothing: on macOS, Quit ends the process right after it
//!    (RUNE-GAPS.md), and then even this record may be lost.
//! 2. A thread waits for the cleanup; when it's done, or `QUIT_WAIT` passes, the record is
//!    made exact: what cleanup left and why, or, if it didn't finish, what was still partly
//!    there. `main` waits for that thread after Rune's loop ends, when no window is on screen
//!    ([`finish`]).
//! 3. The next launch reads the record on the startup thread ([`look`]) and offers only what's
//!    still as Arbor made it; the first window says it, with a button that shows the item, and
//!    only once that frame has been presented (or the user typed or started an operation) is
//!    what was read removed from the record ([`forget`]).
//!
//! The record is user data (it can't be re-derived), a small text file beside the settings:
//! one item per line, its path and why, separated by a tab, each escaped so any path (tabs,
//! line breaks, bytes that aren't UTF-8) round-trips exactly; the note shows it lossily.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use crate::persist;

/// How long the cleanup of an operation cut short by quitting may take, after the window has
/// gone.
pub const QUIT_WAIT: Duration = Duration::from_secs(2);

/// Why an item is in the record when the cleanup didn't get to it.
pub const UNFINISHED: &str = "Arbor quit before it could remove it";

/// Something an operation made and left, its mark as it made it (`ops::still_made`), and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leftover {
    pub path: PathBuf,
    pub mark: String,
    pub why: String,
}

/// Where the record lives (beside `config.toml`).
pub fn file() -> Option<PathBuf> {
    crate::config::path().and_then(|p| p.parent().map(|d| d.join("leftovers.txt")))
}

/// The record at `file` (empty when there's none).
pub fn read(file: &Path) -> Vec<Leftover> {
    let Ok(text) = std::fs::read_to_string(file) else { return Vec::new() };
    text.lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            let (path, mark, why) = (f.next()?, f.next()?, f.next()?);
            let why = String::from_utf8_lossy(&unescape(why)?).into_owned();
            Some(Leftover { path: decode_path(path)?, mark: mark.to_string(), why })
        })
        .collect()
}

/// One entry's line: its path exactly as the OS has it (any bytes, tabs and line breaks
/// included), its mark and why, each escaped so the line is plain text.
fn line(l: &Leftover) -> String {
    format!("{}\t{}\t{}\n", encode_path(&l.path), escape(l.mark.as_bytes()), escape(l.why.as_bytes()))
}

/// Bytes as text: printable ASCII stays (but `%`), anything else is `%XX`.
fn escape(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        if (0x20..0x7f).contains(&b) && b != b'%' {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

/// [`escape`] undone; `None` for a malformed escape.
fn unescape(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(s.get(i + 1..i + 3)?, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

/// A path, losslessly: its bytes on Unix; on Windows its UTF-16 units (an unpaired surrogate
/// included) as `%uXXXX` where they aren't printable ASCII.
#[cfg(unix)]
fn encode_path(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    escape(p.as_os_str().as_bytes())
}

#[cfg(unix)]
fn decode_path(s: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(unescape(s)?)))
}

#[cfg(windows)]
fn encode_path(p: &Path) -> String {
    use std::os::windows::ffi::OsStrExt;
    let mut s = String::new();
    for u in p.as_os_str().encode_wide() {
        if (0x20..0x7f).contains(&u) && u != u16::from(b'%') {
            s.push(char::from(u as u8));
        } else {
            s.push_str(&format!("%u{u:04X}"));
        }
    }
    s
}

#[cfg(windows)]
fn decode_path(s: &str) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if s.get(i + 1..i + 2)? != "u" {
                return None;
            }
            out.push(u16::from_str_radix(s.get(i + 2..i + 6)?, 16).ok()?);
            i += 6;
        } else {
            out.push(u16::from(b[i]));
            i += 1;
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_wide(&out)))
}

/// What the startup read found in the record: everything in it (to forget once said), and
/// what to say, the items still as Arbor made them.
#[derive(Clone, Debug)]
pub struct Seen {
    pub file: PathBuf,
    pub read: Vec<Leftover>,
    pub offered: Vec<Leftover>,
}

/// The record at `file`, left in place until the first window has shown what it says
/// ([`forget`]): a crash before then keeps it for the next launch. Only items still as Arbor
/// made them are offered (not gone, not replaced since). Disk I/O: the startup thread's.
pub fn look(file: &Path) -> Option<Seen> {
    let read = read(file);
    if read.is_empty() {
        return None;
    }
    let offered = read.iter().filter(|l| crate::ops::still_made(&l.path, &l.mark)).cloned().collect();
    Some(Seen { file: file.to_path_buf(), read, offered })
}

/// The record's entries `seen` read, removed once said (on a thread of its own: never the UI
/// thread); anything another Arbor added since stays.
pub fn forget(seen: Seen) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let _ = persist::locked(&seen.file, || {
            let left: Vec<Leftover> = read(&seen.file).into_iter().filter(|l| !seen.read.contains(l)).collect();
            if left.is_empty() {
                return match std::fs::remove_file(&seen.file) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                    _ => Ok(()),
                };
            }
            let text: String = left.iter().map(line).collect();
            persist::replace(&seen.file, text.as_bytes(), persist::Durability::Plain)
        });
    })
}

/// [`look`], then [`forget`] at once (tests: a launch whose note was shown).
#[cfg(test)]
pub fn take(file: &Path) -> Vec<Leftover> {
    let Some(seen) = look(file) else { return Vec::new() };
    let offered = seen.offered.clone();
    let _ = forget(seen).join();
    offered
}

/// The first window's note for `left`.
pub fn note(left: &[Leftover]) -> Option<String> {
    let first = left.first()?;
    let name = first.path.file_name().map_or_else(|| first.path.display().to_string(), |n| n.to_string_lossy().into_owned());
    let dir = first.path.parent().map_or_else(String::new, crate::fs::pretty);
    let more = if left.len() > 1 { format!(" (and {} more)", left.len() - 1) } else { String::new() };
    Some(format!("Arbor quit during a file operation and left “{name}” partly done in {dir}: {}{more}", first.why))
}

/// One change to a record: entries added (replacing any for the same path) and paths dropped.
struct Change {
    file: PathBuf,
    add: Vec<Leftover>,
    drop: Vec<PathBuf>,
}

/// Makes change `c` to its record, under the record's lock: on the quit thread (never the UI
/// thread, never the close callback).
fn apply(c: Change) {
    #[cfg(test)]
    {
        if let Ok(mut w) = WRITES.lock() {
            w.push((c.file.clone(), std::thread::current().name().unwrap_or("").to_string()));
        }
        // A disk that never answers.
        let stall = STALL.lock().ok().and_then(|mut g| g.iter().position(|(f, _)| *f == c.file).map(|i| g.remove(i)));
        if let Some((_, go)) = stall {
            let _ = go.recv();
        }
    }
    let _ = persist::locked(&c.file, || {
        let mut all = read(&c.file);
        all.retain(|l| !c.drop.contains(&l.path) && !c.add.iter().any(|a| a.path == l.path));
        all.extend(c.add);
        if all.is_empty() {
            return match std::fs::remove_file(&c.file) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let text: String = all.iter().map(line).collect();
        persist::replace(&c.file, text.as_bytes(), persist::Durability::Plain)
    });
}

/// Test only: each record write, and the thread that made it.
#[cfg(test)]
pub(crate) static WRITES: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());

/// Test only: a record whose next write waits until told (a stuck disk).
#[cfg(test)]
pub(crate) static STALL: Mutex<Vec<(PathBuf, std::sync::mpsc::Receiver<()>)>> = Mutex::new(Vec::new());

/// How much past its deadline `main` waits for the quit thread's record write.
pub const WRITE_MARGIN: Duration = Duration::from_millis(250);

/// The quit thread's word that it's done, and its deadline, for `main` to wait on once no
/// window is on screen.
static QUITTING: Mutex<Vec<(std::sync::mpsc::Receiver<()>, std::time::Instant)>> = Mutex::new(Vec::new());

/// The last window is closing with `run` going (already cancelled): a thread records what it
/// has left so far in `file` (provisionally: the copy being made), waits up to `wait` for its
/// cleanup, then makes the record exact. Returns at once: the close callback does no record
/// I/O.
pub fn quitting(run: crate::ops::Running, file: PathBuf, wait: Duration) {
    let p = run.progress.clone();
    // What cleanup couldn't remove; and, for a cleanup that hasn't finished, what the copy
    // confirmed it made, by its top items (a folder stands for what's in it). All from memory.
    let kept = |p: &crate::ops::Progress| -> Vec<Leftover> {
        p.kept
            .lock()
            .map(|k| k.iter().map(|(path, mark, why)| Leftover { path: path.clone(), mark: mark.clone(), why: why.clone() }).collect())
            .unwrap_or_default()
    };
    let unfinished = |p: &crate::ops::Progress| -> Vec<Leftover> {
        let made = p.created.lock().map(|c| c.clone()).unwrap_or_default();
        made.iter()
            .filter(|(path, _)| !made.iter().any(|(other, _)| other != path && path.starts_with(other)))
            .map(|(path, mark)| Leftover { path: path.clone(), mark: mark.clone(), why: UNFINISHED.into() })
            .collect()
    };
    let mut provisional = kept(&p);
    provisional.extend(unfinished(&p));
    let deadline = std::time::Instant::now() + wait;
    let (done, done_rx) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new().name("arbor-quit".into()).spawn(move || {
        if !provisional.is_empty() {
            apply(Change { file: file.clone(), add: provisional.clone(), drop: Vec::new() });
        }
        let finished = run.wait(deadline.saturating_duration_since(std::time::Instant::now())).is_some();
        // From here on, only what's in memory: no look at the disk past the deadline.
        let mut sure = kept(&p);
        if !finished {
            // Still partly there when the time ran out: what the copy made and hadn't cleaned up.
            sure.extend(unfinished(&p));
        }
        let drop = provisional.iter().map(|l| l.path.clone()).filter(|p| !sure.iter().any(|s| s.path == *p)).collect();
        apply(Change { file, add: sure, drop });
        let _ = done.send(());
    });
    if thread.is_ok()
        && let Ok(mut all) = QUITTING.lock()
    {
        all.push((done_rx, deadline));
    }
}

/// Waits for the quit thread, if one started, to write the record: at most until its deadline
/// and [`WRITE_MARGIN`] more, so Arbor exits in that time even when the disk doesn't answer
/// (what wasn't written then is lost). Called by `main` after Rune's loop ends: no window is on
/// screen by then.
pub fn finish() {
    let all = QUITTING.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
    for (done, deadline) in all {
        let _ = done.recv_timeout((deadline + WRITE_MARGIN).saturating_duration_since(std::time::Instant::now()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Any path goes into the record and comes back byte for byte (review: names with a tab,
    /// a line break or bytes that aren't UTF-8 were left out of the record).
    #[test]
    fn any_path_round_trips_through_the_record() {
        let d = std::env::temp_dir().join(format!("arbor-leftovers-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut names: Vec<std::ffi::OsString> =
            vec!["a\tb".into(), "a\nb".into(), "a\rb".into(), "50% off".into(), "%41".into(), "é 💾".into()];
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            names.push(std::ffi::OsString::from_vec(vec![b'a', 255, b'b']));
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            names.push(std::ffi::OsString::from_wide(&[u16::from(b'a'), 0xD800, u16::from(b'b')]));
        }
        let all: Vec<Leftover> = names
            .iter()
            .map(|n| Leftover { path: d.join(n), mark: "f:1:2:3:4".into(), why: "it couldn't be removed\t(x)".into() })
            .collect();
        let file = d.join("leftovers.txt");
        apply(Change { file: file.clone(), add: all.clone(), drop: Vec::new() });
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text.lines().count(), all.len(), "one line each: {text:?}");
        assert_eq!(read(&file), all);
        let _ = std::fs::remove_dir_all(&d);
    }
}
