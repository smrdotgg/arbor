//! Compress and Extract Here on the operation worker: progress in bytes, Esc to cancel, never
//! overwriting, undone by trashing what they made (only while it's unchanged) and redone by
//! taking it back out of the Trash, as a copy is. The formats and the safety rules for what an
//! archive may make are in `crate::archive`.
//!
//! - **Compress** writes the `.zip` under a hidden temporary name beside the items, then renames
//!   it into place with the no-replace rename under a free name ("Photos.zip", "Photos 2.zip").
//!   Folders are walked without following links (Unix: through open folder descriptors,
//!   `dirfd.rs`; a link is stored as a link) and never into another disk (`mounts::Fence`), which
//!   is left out and said. A cancelled or failed archive's temporary file is removed only while
//!   it's still the file Arbor made (by identity).
//! - **Extract Here** makes a folder named after the archive (or after the one folder it holds)
//!   beside it, under a free name, and writes into it (`archive::out`). Cancelled or failed, it
//!   removes exactly what it made, each item only while it's still what it made
//!   ([`unmake`]), so a damaged archive leaves nothing half-extracted.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};

use super::*;
use crate::archive::{self, Error as AErr, Format, Kind as AKind, Plan, out::Out};

#[cfg(test)]
mod tests;

/// Deflate's level for Compress (see DECISIONS.md, 2026-11-11, for the measurements).
const LEVEL: u32 = 3;

/// Extensions of formats that are already compressed: stored as they are, since deflate would
/// spend time to save next to nothing.
const PACKED: &[&str] = &[
    "zip", "gz", "tgz", "bz2", "xz", "zst", "lz4", "7z", "rar", "jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "avif", "jxl", "mp3",
    "m4a", "aac", "ogg", "opus", "flac", "mp4", "m4v", "mov", "mkv", "webm", "avi", "docx", "xlsx", "pptx", "odt", "ods", "odp", "pages",
    "numbers", "key", "epub", "jar", "apk", "ipa", "dmg", "woff", "woff2",
];

fn deflates(name: &[u8]) -> bool {
    let n = String::from_utf8_lossy(name);
    match n.rfind('.') {
        Some(i) => !PACKED.contains(&n[i + 1..].to_ascii_lowercase().as_str()),
        None => true,
    }
}

/// What Compress left out, gathered for one note each.
#[derive(Default)]
struct Left {
    mounts: Vec<PathBuf>,
    special: Vec<PathBuf>,
    links: Vec<PathBuf>,
    changed: Vec<PathBuf>,
}

impl Left {
    fn notes(&self, into: &mut Vec<String>) {
        let mut say = |v: &[PathBuf], why: &str| {
            if let Some(first) = v.first() {
                let more = if v.len() > 1 { format!(" (and {} more)", v.len() - 1) } else { String::new() };
                into.push(format!("“{}” was left out: {why}{more}", name_of(first)));
            }
        };
        say(&self.mounts, "another disk is mounted there");
        say(&self.special, "devices, sockets and FIFOs aren't archived");
        say(&self.links, "links aren't archived on Windows");
        say(&self.changed, "it changed while it was being read");
    }
}

/// The name a compressed archive of `sources` gets: "Photos.zip" for one item, else
/// "Archive.zip".
pub fn archive_name(sources: &[PathBuf]) -> String {
    match sources {
        [one] => format!("{}.zip", name_of(one)),
        _ => "Archive.zip".into(),
    }
}

/// What an archive error says.
fn said(e: &AErr) -> String {
    e.to_string()
}

impl Worker<'_> {
    /// [`Op::Compress`]: `sources` (all in `dir`) into one `.zip` in `dir`.
    pub(super) fn compress(&mut self, sources: Vec<PathBuf>, dir: PathBuf, out: &mut Outcome) {
        self.p.items_total.store(sources.len(), Relaxed);
        let mut keep = Vec::new();
        for s in sources {
            match self.changed(&s) {
                Some(why) => out.errors.push(why),
                None => keep.push(s),
            }
        }
        if keep.is_empty() {
            return;
        }
        let name = archive_name(&keep);
        let mounts = self.mounts().clone();
        let total: u64 = keep.iter().map(|s| self.size_of(s, &crate::mounts::Fence::below(s, &mounts))).sum();
        if self.cancelled() {
            out.said = Some(format!("Cancelled: “{name}” wasn't made"));
            return;
        }
        self.p.bytes_total.store(total, Relaxed);
        let (temp, file) = match make_temp(&dir, &name) {
            Ok(t) => t,
            Err(e) => {
                out.errors.push(format!("Couldn't make “{name}”: {e}"));
                return;
            }
        };
        let temp_id = file_id_of(&file);
        let mut left = Left::default();
        let r = self.write_zip(file, &keep, &mounts, &mut left);
        let fail = |why: String, out: &mut Outcome| out.errors.push(with_kept(why, remove_temp(&temp, temp_id)));
        let file = match r {
            Ok(f) => f,
            Err(AErr::Cancelled) => {
                let kept = remove_temp(&temp, temp_id).map_or_else(String::new, |k| format!("; {k}"));
                out.said = Some(format!("Cancelled: “{name}” wasn't made{kept}"));
                return;
            }
            Err(e) => {
                fail(format!("Couldn't compress into “{name}”: {}", said(&e)), out);
                return;
            }
        };
        drop(file);
        // Into place, under a free name, never over anything.
        let mut placed = None;
        for _ in 0..TRIES {
            let to = free_name(&dir, std::ffi::OsStr::new(&name), false);
            self.before_act(&to);
            match noclobber::rename(&temp, &to) {
                Ok(()) => {
                    placed = Some(to);
                    break;
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => {
                    fail(format!("Couldn't name “{name}”: {e}"), out);
                    return;
                }
            }
        }
        let Some(to) = placed else {
            fail(format!("Couldn't name “{name}”: every free name was taken before it could be used"), out);
            return;
        };
        self.p.items_done.store(keep.len(), Relaxed);
        left.notes(&mut out.notes);
        out.undo = Undo::record_or_say(What::Compress, [(to.clone(), None)], &mut out.errors);
        if out.errors.is_empty() {
            let what = match keep.as_slice() {
                [one] => format!("“{}”", name_of(one)),
                v => format!("{} items", v.len()),
            };
            out.said = Some(format!("Compressed {what} into “{}”", name_of(&to)));
        }
        out.results.push(to);
    }

    /// Writes every item of `sources` into a ZIP in `file`; the file, finished.
    fn write_zip(&mut self, file: File, sources: &[PathBuf], mounts: &crate::mounts::Mounts, left: &mut Left) -> Result<File, AErr> {
        let mut w = archive::zip::Writer::new(file, LEVEL);
        for src in sources {
            if self.cancelled() {
                return Err(AErr::Cancelled);
            }
            // Checked again right before it's read.
            if let Some(why) = self.changed(src) {
                return Err(AErr::Io(io::Error::other(why)));
            }
            let fence = crate::mounts::Fence::below(src, mounts);
            let top = src.file_name().ok_or_else(|| AErr::Io(io::Error::other("no file name")))?;
            self.add_item(&mut w, src, &os_bytes(top), &fence, left)?;
            self.p.items_done.fetch_add(1, Relaxed);
        }
        Ok(w.finish()?)
    }

    /// Total bytes of the files Compress will read under `p`: no links followed, no other disk.
    fn size_of(&self, p: &Path, fence: &crate::mounts::Fence) -> u64 {
        if self.cancelled() {
            return 0;
        }
        let Ok(m) = p.symlink_metadata() else { return 0 };
        if m.is_dir() {
            if fence.crosses(p, &m) {
                return 0;
            }
            std::fs::read_dir(p).map_or(0, |rd| rd.flatten().map(|de| self.size_of(&de.path(), fence)).sum())
        } else if m.is_file() {
            m.len()
        } else {
            0
        }
    }

    /// A file's data into the archive, ticking the progress; `false` from the tick stops.
    #[allow(clippy::too_many_arguments)]
    fn add_file(
        &mut self,
        w: &mut archive::zip::Writer,
        name: &[u8],
        f: &mut File,
        size: u64,
        mtime: Option<SystemTime>,
        perm: u32,
    ) -> Result<(), AErr> {
        let utf8 = std::str::from_utf8(name).is_ok();
        let deflate = deflates(name);
        self.cancel_hook(Path::new(&*String::from_utf8_lossy(name)));
        w.file(name, utf8, f, size, mtime, perm, deflate, &mut |k| {
            self.tick(k);
            !self.cancelled()
        })
    }

    #[cfg(unix)]
    fn add_item(
        &mut self,
        w: &mut archive::zip::Writer,
        src: &Path,
        name: &[u8],
        fence: &crate::mounts::Fence,
        left: &mut Left,
    ) -> Result<(), AErr> {
        use std::os::unix::fs::MetadataExt;
        let m = src.symlink_metadata()?;
        let ft = m.file_type();
        let mtime = m.modified().ok();
        if ft.is_dir() {
            let fd = crate::dirfd::open_dir(libc::AT_FDCWD, &crate::ffi::cstr(src)?, false)?;
            if crate::dirfd::node(&crate::dirfd::fstat(&fd)?) != (m.dev(), m.ino()) {
                left.changed.push(src.to_path_buf());
                return Ok(());
            }
            w.dir(&[name, b"/"].concat(), std::str::from_utf8(name).is_ok(), mtime, m.mode())?;
            self.add_dir(w, &fd, src, name, fence, left)
        } else if ft.is_file() {
            let mut f = open_no_follow(libc::AT_FDCWD, &crate::ffi::cstr(src)?)?;
            let fm = f.metadata()?;
            if !fm.is_file() || (fm.dev(), fm.ino()) != (m.dev(), m.ino()) {
                left.changed.push(src.to_path_buf());
                return Ok(());
            }
            self.add_file(w, name, &mut f, fm.len(), fm.modified().ok(), fm.mode())
        } else if ft.is_symlink() {
            use std::os::unix::ffi::OsStrExt;
            let target = std::fs::read_link(src)?;
            Ok(w.link(name, std::str::from_utf8(name).is_ok(), target.as_os_str().as_bytes(), mtime)?)
        } else {
            left.special.push(src.to_path_buf());
            Ok(())
        }
    }

    /// Everything in the open folder `fd` (at `path`, named `name` in the archive), in name
    /// order, each entry `lstat`ed and opened relative to it without following a link.
    #[cfg(unix)]
    // `mode_t` is u16 on macOS and u32 on Linux.
    #[allow(clippy::unnecessary_cast)]
    fn add_dir(
        &mut self,
        w: &mut archive::zip::Writer,
        fd: &std::os::fd::OwnedFd,
        path: &Path,
        name: &[u8],
        fence: &crate::mounts::Fence,
        left: &mut Left,
    ) -> Result<(), AErr> {
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let mut names = crate::dirfd::names(fd)?;
        names.sort();
        for n in names {
            if self.cancelled() {
                return Err(AErr::Cancelled);
            }
            let child = path.join(std::ffi::OsStr::from_bytes(n.to_bytes()));
            let st = match crate::dirfd::stat_at(fd, &n) {
                Ok(st) => st,
                Err(e) if e.kind() == ErrorKind::NotFound => continue, // gone since the folder was read
                Err(e) => return Err(AErr::Io(io::Error::new(e.kind(), format!("“{}”: {e}", child.display())))),
            };
            let entry = [name, b"/", n.to_bytes()].concat();
            let utf8 = std::str::from_utf8(&entry).is_ok();
            let mtime = stat_mtime(&st);
            #[allow(clippy::unnecessary_cast)]
            let (dev, mode) = (st.st_dev as u64, st.st_mode as u32);
            match mode & libc::S_IFMT as u32 {
                m if m == libc::S_IFDIR as u32 => {
                    if fence.crosses_dev(&child, Some(dev), true) {
                        left.mounts.push(child);
                        continue;
                    }
                    let sub = match crate::dirfd::open_dir(fd.as_raw_fd(), &n, false) {
                        Ok(sub) => sub,
                        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR | libc::ENOENT)) => {
                            left.changed.push(child);
                            continue;
                        }
                        Err(e) => return Err(AErr::Io(io::Error::new(e.kind(), format!("“{}”: {e}", child.display())))),
                    };
                    if !crate::dirfd::same(&crate::dirfd::fstat(&sub)?, &st) {
                        left.changed.push(child);
                        continue;
                    }
                    w.dir(&[&entry[..], b"/"].concat(), utf8, mtime, mode)?;
                    self.add_dir(w, &sub, &child, &entry, fence, left)?;
                }
                m if m == libc::S_IFREG as u32 => {
                    let mut f = match open_no_follow(fd.as_raw_fd(), &n) {
                        Ok(f) => f,
                        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOENT)) => {
                            left.changed.push(child);
                            continue;
                        }
                        Err(e) => return Err(AErr::Io(io::Error::new(e.kind(), format!("“{}”: {e}", child.display())))),
                    };
                    let now = crate::dirfd::fstat(&std::os::fd::OwnedFd::from(f.try_clone()?))?;
                    if !crate::dirfd::same(&now, &st) || now.st_mode & libc::S_IFMT != libc::S_IFREG {
                        left.changed.push(child);
                        continue;
                    }
                    #[allow(clippy::unnecessary_cast)]
                    self.add_file(w, &entry, &mut f, now.st_size as u64, stat_mtime(&now), mode)?;
                }
                m if m == libc::S_IFLNK as u32 => {
                    let target = read_link_at(fd.as_raw_fd(), &n)?;
                    w.link(&entry, utf8, &target, mtime)?;
                }
                _ => left.special.push(child),
            }
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn add_item(
        &mut self,
        w: &mut archive::zip::Writer,
        src: &Path,
        name: &[u8],
        fence: &crate::mounts::Fence,
        left: &mut Left,
    ) -> Result<(), AErr> {
        if self.cancelled() {
            return Err(AErr::Cancelled);
        }
        let m = src.symlink_metadata()?;
        let ft = m.file_type();
        let utf8 = std::str::from_utf8(name).is_ok();
        if ft.is_symlink() {
            left.links.push(src.to_path_buf());
        } else if ft.is_dir() {
            if fence.crosses(src, &m) {
                left.mounts.push(src.to_path_buf());
                return Ok(());
            }
            w.dir(&[name, b"/"].concat(), utf8, m.modified().ok(), 0o755)?;
            let mut kids: Vec<_> = std::fs::read_dir(src)?.collect::<Result<_, _>>()?;
            kids.sort_by_key(|d| d.file_name());
            for de in kids {
                let n = de.file_name();
                self.add_item(w, &de.path(), &[name, b"/", &os_bytes(&n)].concat(), fence, left)?;
            }
        } else if ft.is_file() {
            let mut f = File::open(src)?;
            let fm = f.metadata()?;
            let perm = if m.permissions().readonly() { 0o444 } else { 0o644 };
            self.add_file(w, name, &mut f, fm.len(), fm.modified().ok(), perm)?;
        } else {
            left.special.push(src.to_path_buf());
        }
        Ok(())
    }

    // ---- extract -----------------------------------------------------------------------------

    /// [`Op::Extract`]: each archive into a folder beside it.
    pub(super) fn extract(&mut self, archives: Vec<PathBuf>, out: &mut Outcome) {
        self.p.items_total.store(archives.len(), Relaxed);
        let mut steps = Vec::new();
        for a in &archives {
            if self.cancelled() {
                break;
            }
            if let Some(why) = self.changed(a) {
                out.errors.push(why);
                self.p.items_done.fetch_add(1, Relaxed);
                continue;
            }
            match self.extract_one(a, &mut out.notes) {
                Ok((root, print)) => {
                    out.results.push(root.clone());
                    steps.push(Step { at: root, print, back: None, split: None });
                }
                Err(_) if self.cancelled() => {}
                Err(why) => out.errors.push(format!("Couldn't extract “{}”: {why}", name_of(a))),
            }
            self.p.items_done.fetch_add(1, Relaxed);
        }
        if self.cancelled() {
            out.said = Some(match out.results.len() {
                0 => "Cancelled: nothing was extracted".into(),
                n => format!("Cancelled after extracting {n} archive{}", if n == 1 { "" } else { "s" }),
            });
        } else if out.errors.is_empty() {
            out.said = Some(match (archives.as_slice(), out.results.as_slice()) {
                ([a], [r]) => format!("Extracted “{}” into “{}”", name_of(a), name_of(r)),
                (_, r) => format!("Extracted {} archives", r.len()),
            });
        }
        if !steps.is_empty() {
            out.undo = Some(Undo { what: What::Extract, redo: false, steps });
        }
    }

    /// Extracts `archive` beside it: the folder it made, fingerprinted with everything inside,
    /// or why not (what it made removed again).
    fn extract_one(&mut self, archive: &Path, notes: &mut Vec<String>) -> Result<(PathBuf, Print), String> {
        let name = name_of(archive);
        let fmt = Format::of(&name).ok_or("it isn't an archive Arbor can extract (.zip, .tar, .tar.gz)")?;
        let parent = archive.parent().ok_or("it has no folder")?.to_path_buf();
        let file = File::open(archive).map_err(|e| e.to_string())?;
        let len = file.metadata().map_err(|e| e.to_string())?.len();
        self.p.bytes_done.store(0, Relaxed);
        let (entries, zl) = self.list_archive(fmt, &file, len).map_err(|e| said(&e))?;
        let plan = archive::plan(&name, len, &entries, crate::rename::Platform::current()).map_err(|e| said(&e))?;
        if let Some(free) = archive::free_space(&parent)
            && plan.total > free
        {
            return Err(format!(
                "it unpacks to {}, and only {} is free on the disk",
                crate::fs::fmt_size(plan.total),
                crate::fs::fmt_size(free)
            ));
        }
        self.p.bytes_total.store(plan.total, Relaxed);
        self.p.bytes_done.store(0, Relaxed);
        // The folder, under a free name, made exclusively.
        let mut made_root = None;
        for _ in 0..TRIES {
            let root = free_name(&parent, std::ffi::OsStr::new(&plan.root), false);
            self.before_act(&root);
            match std::fs::create_dir(&root) {
                Ok(()) => {
                    made_root = Some(root);
                    break;
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("couldn't make a folder for it: {e}")),
            }
        }
        let root = made_root.ok_or("every free name for its folder was taken before it could be used")?;
        // Its identity at once (on Windows read through a handle on the path), before anything
        // else can take its place.
        let meta = root.symlink_metadata().map_err(|e| e.to_string())?;
        let root_print = Print::of(&root, &meta);
        self.after_made(&root);
        let mut made = vec![Made { src: PathBuf::new(), dst: root.clone(), dir: true, was: None, made: Some(root_print.clone()) }];
        let Some(id) = root_print.id else {
            return Err(with_kept("its folder couldn't be identified".into(), unmake(&made)));
        };
        let mut o = match Out::new(root.clone(), id) {
            Ok(o) => o,
            Err(e) => return Err(with_kept(said(&e), unmake(&made))),
        };
        let r = self.write_entries(&mut o, fmt, &file, zl.as_ref(), &entries, &plan);
        made.extend(o.made.drain(..).map(|m| Made {
            src: PathBuf::new(),
            dir: m.dir,
            made: Some(Print::with_id(&m.meta, m.id)),
            dst: m.path,
            was: None,
        }));
        match r {
            Ok(dups) => {
                notes.extend(plan.notes.iter().cloned());
                if dups > 0 {
                    notes.push(format!(
                        "{dups} name{} in it already taken (twice in the archive, or by case) and got free ones (“name 2”)",
                        if dups == 1 { " was" } else { "s were" }
                    ));
                }
            }
            Err(AErr::Cancelled) => return Err(with_kept("cancelled".into(), unmake(&made))),
            Err(e) => return Err(with_kept(said(&e), unmake(&made))),
        }
        let print =
            self.print_all(&root).map_err(|e| format!("it was extracted into “{}”, but that can't be read back: {e}", name_of(&root)))?;
        Ok((root, print))
    }

    /// The archive's list (for a ZIP, with where each entry's data is). A gzipped tar is
    /// decompressed for it, with its compressed bytes as the progress.
    fn list_archive(&mut self, fmt: Format, file: &File, len: u64) -> Result<(Vec<archive::Entry>, Option<archive::zip::Listing>), AErr> {
        let p = self.p;
        let stop = || p.cancel.load(Relaxed);
        match fmt {
            Format::Zip => {
                let mut l = archive::zip::list(file, len, &stop)?;
                Ok((std::mem::take(&mut l.entries), Some(l)))
            }
            Format::Tar => {
                (&*file).seek(SeekFrom::Start(0))?;
                Ok((archive::tar::list(BufReader::with_capacity(256 * 1024, file), len, &stop)?, None))
            }
            Format::TarGz => {
                (&*file).seek(SeekFrom::Start(0))?;
                self.p.bytes_total.store(len, Relaxed);
                let n = Arc::new(AtomicU64::new(0));
                let counted = archive::tar::Counted { r: file, n: n.clone() };
                let gz = flate2::read::MultiGzDecoder::new(BufReader::with_capacity(256 * 1024, counted));
                let waker = self.waker;
                let last = std::cell::Cell::new(Instant::now());
                let stop = || {
                    p.bytes_done.store(n.load(Relaxed), Relaxed);
                    if last.get().elapsed() >= Duration::from_millis(50) {
                        last.set(Instant::now());
                        if let Some(w) = waker {
                            w();
                        }
                    }
                    p.cancel.load(Relaxed)
                };
                Ok((archive::tar::list(gz, len, &stop)?, None))
            }
        }
    }

    /// Writes `plan`'s entries into `o`: files and folders in archive order, then links, then
    /// folders' times (deepest first, once nothing more changes inside them). How many names
    /// were taken and got free ones.
    fn write_entries(
        &mut self,
        o: &mut Out,
        fmt: Format,
        file: &File,
        zl: Option<&archive::zip::Listing>,
        entries: &[archive::Entry],
        plan: &Plan,
    ) -> Result<usize, AErr> {
        let mut buf = vec![0u8; 256 * 1024];
        let mut dups = 0;
        let mut times: Vec<(&[String], SystemTime)> = Vec::new();
        let mut links = Vec::new();
        match (fmt, zl) {
            (Format::Zip, Some(zl)) => {
                for it in &plan.items {
                    if self.cancelled() {
                        return Err(AErr::Cancelled);
                    }
                    match &it.kind {
                        AKind::Dir => o.dir(&it.rel)?,
                        AKind::File { .. } => {
                            let mut d = zl.open(file, it.index)?;
                            let mut nf = o.file(&it.rel, it.mode)?;
                            dups += usize::from(nf.renamed);
                            self.cancel_hook(&nf.path);
                            let poured = self.pour(&mut |b| Ok(d.read(b)?), &mut nf.file, &mut buf).and_then(|()| d.finish());
                            match poured {
                                Ok(()) => o.done(nf, it.mtime)?,
                                Err(e) => {
                                    o.abandon(nf);
                                    return Err(e);
                                }
                            }
                        }
                        AKind::Link(_) => links.push(it),
                        AKind::Skip(_) => {}
                    }
                    if let (AKind::Dir, Some(t)) = (&it.kind, it.mtime) {
                        times.push((&it.rel, t));
                    }
                }
            }
            _ => {
                // The tar again, from the start, each entry matched against the list.
                (&*file).seek(SeekFrom::Start(0))?;
                let raw = BufReader::with_capacity(256 * 1024, file);
                let src: Box<dyn Read + '_> =
                    if fmt == Format::TarGz { Box::new(flate2::read::MultiGzDecoder::new(raw)) } else { Box::new(raw) };
                let mut rd = archive::tar::Reader::new(src);
                let mut next = plan.items.iter().peekable();
                let changed = || AErr::Corrupt("it changed while it was being extracted".into());
                for (i, want) in entries.iter().enumerate() {
                    if self.cancelled() {
                        return Err(AErr::Cancelled);
                    }
                    let e = rd.next()?.ok_or_else(changed)?;
                    if e.name != want.name || e.kind != want.kind {
                        return Err(changed());
                    }
                    let Some(it) = next.next_if(|it| it.index == i) else { continue };
                    match &it.kind {
                        AKind::Dir => o.dir(&it.rel)?,
                        AKind::File { .. } => {
                            let mut nf = o.file(&it.rel, it.mode)?;
                            dups += usize::from(nf.renamed);
                            self.cancel_hook(&nf.path);
                            match self.pour(&mut |b| rd.read_data(b), &mut nf.file, &mut buf) {
                                Ok(()) => o.done(nf, it.mtime)?,
                                Err(e) => {
                                    o.abandon(nf);
                                    return Err(e);
                                }
                            }
                        }
                        AKind::Link(_) => links.push(it),
                        AKind::Skip(_) => {}
                    }
                    if let (AKind::Dir, Some(t)) = (&it.kind, it.mtime) {
                        times.push((&it.rel, t));
                    }
                }
            }
        }
        for it in links {
            if self.cancelled() {
                return Err(AErr::Cancelled);
            }
            if let AKind::Link(target) = &it.kind {
                dups += usize::from(o.link(&it.rel, target, it.mtime)?);
            }
        }
        times.sort_by_key(|(rel, _)| std::cmp::Reverse(rel.len()));
        for (rel, t) in times {
            o.dir_time(rel, t)?;
        }
        if let Some(t) = plan.root_mtime {
            o.dir_time(&[], t)?;
        }
        Ok(dups)
    }

    /// Copies what `read` gives into `to`, ticking the progress; stops on a cancel.
    fn pour(&mut self, read: &mut dyn FnMut(&mut [u8]) -> Result<usize, AErr>, to: &mut File, buf: &mut [u8]) -> Result<(), AErr> {
        loop {
            let n = read(buf)?;
            if n == 0 {
                return Ok(());
            }
            to.write_all(&buf[..n])?;
            self.tick(n as u64);
            if self.cancelled() {
                return Err(AErr::Cancelled);
            }
        }
    }
}

/// A new, hidden file beside where the archive goes, made exclusively: its path and the file.
fn make_temp(dir: &Path, name: &str) -> io::Result<(PathBuf, File)> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let mut last = None;
    for _ in 0..TRIES {
        let n = N.fetch_add(1, Relaxed);
        let p = dir.join(format!(".{name}.arbor-{}-{n}.part", std::process::id()));
        let mut o = std::fs::OpenOptions::new();
        o.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o666).custom_flags(libc::O_NOFOLLOW);
        }
        match o.open(&p) {
            Ok(f) => return Ok((p, f)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::from(ErrorKind::AlreadyExists)))
}

/// The identity of an open file (`noclobber::file_id`'s).
fn file_id_of(f: &File) -> Option<noclobber::FileId> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        f.metadata().ok().map(|m| (m.dev(), m.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: an open handle and a zeroed out-buffer.
        (unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } != 0)
            .then(|| (u64::from(info.dwVolumeSerialNumber), (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)))
    }
}

/// Removes Compress's temporary file, only while it's still the one it made (`id`); what was
/// left and why otherwise.
fn remove_temp(temp: &Path, id: Option<noclobber::FileId>) -> Option<String> {
    let m = temp.symlink_metadata().ok()?;
    if id.is_some() && noclobber::file_id(temp, &m) == id {
        std::fs::remove_file(temp).err().map(|e| format!("kept “{}”: {e}", name_of(temp)))
    } else {
        Some(format!("kept “{}”: it changed after Arbor made it", name_of(temp)))
    }
}

/// A name's bytes as the archive stores them (Unix: as they are; Windows: UTF-8).
fn os_bytes(n: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        n.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        n.to_string_lossy().into_owned().into_bytes()
    }
}

/// Opens `name` in folder `dir` (or a path, with `AT_FDCWD`) to read: never a link, and never
/// waiting on a FIFO put there since it was looked at.
#[cfg(unix)]
fn open_no_follow(dir: std::os::fd::RawFd, name: &std::ffi::CStr) -> io::Result<File> {
    use std::os::fd::FromRawFd;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    // SAFETY: a NUL-terminated name; a non-negative result is a descriptor we own.
    let fd = unsafe { libc::openat(dir, name.as_ptr(), flags) };
    if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(File::from(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })) }
}

/// The target of link `name` in folder `dir`.
#[cfg(unix)]
fn read_link_at(dir: std::os::fd::RawFd, name: &std::ffi::CStr) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; 4096];
    // SAFETY: a NUL-terminated name and a buffer of the length given.
    let n = unsafe { libc::readlinkat(dir, name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(n as usize);
    Ok(buf)
}

#[cfg(unix)]
fn stat_mtime(st: &libc::stat) -> Option<SystemTime> {
    #[allow(clippy::unnecessary_cast)]
    let (s, ns) = (st.st_mtime as i64, st.st_mtime_nsec as u32);
    archive::from_unix(s).map(|t| t + Duration::from_nanos(u64::from(ns)))
}
