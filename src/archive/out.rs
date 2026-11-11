//! Writing an archive's entries into the folder Extract made, never following a link and never
//! replacing anything. On Unix every folder is opened relative to its parent's descriptor with
//! `O_NOFOLLOW` (`dirfd.rs`), and files are created in it with `O_CREAT | O_EXCL | O_NOFOLLOW`:
//! a link in the way, whoever made it, is refused, never followed. A name the archive has twice
//! (or twice to a disk that ignores case) gets a free one ("name 2.txt"). On Windows the same by
//! path: `CREATE_NEW` fails on anything at the name, links included, and Arbor makes no links
//! there.
//!
//! Everything made is recorded ([`Out::made`]) as the handle that made it knows it, for the
//! identity-checked cleanup of a cancelled or failed extraction (`ops::unmake`).

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::Error;

/// Something an extraction made: where, whether it's a folder, and what it was right after
/// (its metadata and identity, from its handle where there is one).
pub struct Made {
    pub path: PathBuf,
    pub dir: bool,
    pub meta: std::fs::Metadata,
    pub id: Option<crate::noclobber::FileId>,
}

/// A file just created, to write.
pub struct NewFile {
    pub file: File,
    pub path: PathBuf,
    /// The name was taken (the archive has it twice): this is the free one it got.
    pub renamed: bool,
}

/// The folder an extraction writes into, and what it has made there.
pub struct Out {
    root: PathBuf,
    /// The open folders from the root down to the last one written in (Unix).
    #[cfg(unix)]
    stack: Vec<(String, std::os::fd::OwnedFd)>,
    /// The folders below the root made so far (Windows).
    #[cfg(not(unix))]
    stack: Vec<String>,
    pub made: Vec<Made>,
}

/// How many free names a taken name tries ("name 2" to "name 100").
const TRIES: u32 = 100;

/// "stem n.ext" for name `name` (the extension kept for files).
fn variant(name: &str, n: u32, keep_ext: bool) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 && keep_ext => format!("{} {n}{}", &name[..i], &name[i..]),
        _ => format!("{name} {n}"),
    }
}

fn not_a_folder(at: &Path) -> Error {
    Error::Corrupt(format!("“{}” is both a file and a folder in it (or a link has its name)", at.display()))
}

#[cfg(unix)]
mod imp {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use super::*;
    use crate::dirfd::open_dir;

    fn cname(s: &str) -> Result<CString, Error> {
        CString::new(s).map_err(|_| Error::Refused("a name has a NUL character".into()))
    }

    fn last_err() -> io::Error {
        io::Error::last_os_error()
    }

    /// Unix's mode for a new file: the archive's bits for its owner (read always), the rest from
    /// the default (0666, or 0777 when the owner may run it, less the umask). Never setuid,
    /// setgid or sticky.
    fn file_mode(mode: Option<u32>) -> libc::mode_t {
        let m = match mode {
            Some(m) => (m & 0o700) | 0o400 | if m & 0o100 != 0 { 0o077 } else { 0o066 },
            None => 0o666,
        };
        m as libc::mode_t
    }

    fn stamp(t: SystemTime) -> libc::timespec {
        let d = t.duration_since(std::time::UNIX_EPOCH);
        let (s, ns) = match d {
            Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
            Err(e) => {
                let d = e.duration();
                let (s, ns) = (-(d.as_secs() as i64), d.subsec_nanos());
                if ns > 0 { (s - 1, 1_000_000_000 - ns) } else { (s, 0) }
            }
        };
        #[allow(clippy::unnecessary_cast)]
        libc::timespec { tv_sec: s as libc::time_t, tv_nsec: ns as _ }
    }

    /// Modified `t`, accessed left as it is.
    fn times(t: SystemTime) -> [libc::timespec; 2] {
        #[allow(clippy::unnecessary_cast)]
        let omit = libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT as _ };
        [omit, stamp(t)]
    }

    impl Out {
        /// The folder at `root`, just made by the caller and known by `id`: opened without
        /// following a link, and refused if it isn't that folder.
        pub fn new(root: PathBuf, id: crate::noclobber::FileId) -> Result<Out, Error> {
            let fd = open_dir(libc::AT_FDCWD, &crate::ffi::cstr(&root)?, false)?;
            if crate::dirfd::node(&crate::dirfd::fstat(&fd)?) != id {
                return Err(Error::Io(io::Error::other("the folder it was extracting into was replaced")));
            }
            Ok(Out { root, stack: vec![(String::new(), fd)], made: Vec::new() })
        }

        fn path(&self, rel: &[String]) -> PathBuf {
            let mut p = self.root.clone();
            p.extend(rel);
            p
        }

        /// Opens (making as needed) the folders `dirs` below the root, each without following a
        /// link; the descriptor of the last.
        fn enter(&mut self, dirs: &[String]) -> Result<i32, Error> {
            let mut k = 0;
            while k < dirs.len() && k + 1 < self.stack.len() && self.stack[k + 1].0 == dirs[k] {
                k += 1;
            }
            self.stack.truncate(k + 1);
            for (i, d) in dirs.iter().enumerate().skip(k) {
                let parent = self.stack.last().expect("the root").1.as_raw_fd();
                let c = cname(d)?;
                // SAFETY: an open folder and a NUL-terminated name.
                let fresh = unsafe { libc::mkdirat(parent, c.as_ptr(), 0o777) } == 0;
                if !fresh {
                    let e = last_err();
                    if e.raw_os_error() != Some(libc::EEXIST) {
                        return Err(Error::Io(e));
                    }
                }
                let fd = match open_dir(parent, &c, false) {
                    Ok(fd) => fd,
                    Err(e) => {
                        // Made but not opened: still recorded, so the cleanup removes it.
                        let path = self.path(&dirs[..=i]);
                        if fresh && let Ok(meta) = path.symlink_metadata() {
                            let id = crate::noclobber::file_id(&path, &meta);
                            self.made.push(Made { path: path.clone(), dir: true, meta, id });
                        }
                        return Err(if matches!(e.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)) {
                            not_a_folder(&path)
                        } else {
                            Error::Io(e)
                        });
                    }
                };
                if fresh {
                    let f = File::from(fd.try_clone()?);
                    let meta = f.metadata()?;
                    let id = crate::dirfd::node(&crate::dirfd::fstat(&fd)?);
                    self.made.push(Made { path: self.path(&dirs[..=i]), dir: true, meta, id: Some(id) });
                }
                self.stack.push((d.clone(), fd));
            }
            Ok(self.stack.last().expect("the root").1.as_raw_fd())
        }

        /// The folder `rel` (and those above it).
        pub fn dir(&mut self, rel: &[String]) -> Result<(), Error> {
            self.enter(rel).map(|_| ())
        }

        /// A new file at `rel` with `mode`'s owner bits (see [`file_mode`]), under a free name if
        /// it's taken.
        pub fn file(&mut self, rel: &[String], mode: Option<u32>) -> Result<NewFile, Error> {
            let (name, dirs) = rel.split_last().expect("a name");
            let dir = self.enter(dirs)?;
            for n in 1..=TRIES {
                let try_name = if n == 1 { name.clone() } else { variant(name, n, true) };
                let c = cname(&try_name)?;
                let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
                // SAFETY: an open folder, a NUL-terminated name; a non-negative result is ours.
                let fd = unsafe { libc::openat(dir, c.as_ptr(), flags, libc::c_uint::from(file_mode(mode))) };
                if fd >= 0 {
                    let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
                    let mut path = self.path(dirs);
                    path.push(&try_name);
                    return Ok(NewFile { file, path, renamed: n > 1 });
                }
                let e = last_err();
                if e.raw_os_error() != Some(libc::EEXIST) {
                    return Err(Error::Io(e));
                }
            }
            Err(Error::Io(io::Error::new(io::ErrorKind::AlreadyExists, format!("every free name for “{name}” was taken"))))
        }

        /// A symbolic link at `rel` to `target` (checked by `plan`), under a free name if it's
        /// taken; whether it got one.
        pub fn link(&mut self, rel: &[String], target: &str, mtime: Option<SystemTime>) -> Result<bool, Error> {
            let (name, dirs) = rel.split_last().expect("a name");
            let dir = self.enter(dirs)?;
            let t = cname(target)?;
            for n in 1..=TRIES {
                let try_name = if n == 1 { name.clone() } else { variant(name, n, false) };
                let c = cname(&try_name)?;
                // SAFETY: NUL-terminated strings and an open folder.
                if unsafe { libc::symlinkat(t.as_ptr(), dir, c.as_ptr()) } == 0 {
                    if let Some(m) = mtime {
                        let ts = times(m);
                        // SAFETY: as above; the link itself, not followed.
                        unsafe { libc::utimensat(dir, c.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
                    }
                    let mut path = self.path(dirs);
                    path.push(&try_name);
                    let meta = path.symlink_metadata()?;
                    let id = crate::noclobber::file_id(&path, &meta);
                    self.made.push(Made { path, dir: false, meta, id });
                    return Ok(n > 1);
                }
                let e = last_err();
                if e.raw_os_error() != Some(libc::EEXIST) {
                    return Err(Error::Io(e));
                }
            }
            Err(Error::Io(io::Error::new(io::ErrorKind::AlreadyExists, format!("every free name for “{name}” was taken"))))
        }

        /// Sets the modified time of folder `rel` (the root when empty).
        pub fn dir_time(&mut self, rel: &[String], t: SystemTime) -> Result<(), Error> {
            let fd = self.enter(rel)?;
            let ts = times(t);
            // SAFETY: an open folder and two timespecs.
            if unsafe { libc::futimens(fd, ts.as_ptr()) } != 0 {
                return Err(Error::Io(last_err()));
            }
            Ok(())
        }
    }

    /// The identity of an open file, as `noclobber::file_id` gives it.
    pub(super) fn id_of(f: &File) -> io::Result<crate::noclobber::FileId> {
        use std::os::unix::fs::MetadataExt;
        let m = f.metadata()?;
        Ok((m.dev(), m.ino()))
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;

    impl Out {
        /// The folder at `root`, just made by the caller and known by `id`.
        pub fn new(root: PathBuf, id: crate::noclobber::FileId) -> Result<Out, Error> {
            if crate::noclobber::dir_id(&root) != Some(id) {
                return Err(Error::Io(io::Error::other("the folder it was extracting into was replaced")));
            }
            Ok(Out { root, stack: Vec::new(), made: Vec::new() })
        }

        fn path(&self, rel: &[String]) -> PathBuf {
            let mut p = self.root.clone();
            p.extend(rel);
            p
        }

        fn enter(&mut self, dirs: &[String]) -> Result<PathBuf, Error> {
            let mut k = 0;
            while k < dirs.len() && k < self.stack.len() && self.stack[k] == dirs[k] {
                k += 1;
            }
            self.stack.truncate(k);
            for i in k..dirs.len() {
                let p = self.path(&dirs[..=i]);
                match std::fs::create_dir(&p) {
                    Ok(()) => {
                        let meta = p.symlink_metadata()?;
                        let id = crate::noclobber::file_id(&p, &meta);
                        self.made.push(Made { path: p, dir: true, meta, id });
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        let m = p.symlink_metadata()?;
                        if !m.is_dir() || m.file_type().is_symlink() {
                            return Err(not_a_folder(&p));
                        }
                    }
                    Err(e) => return Err(Error::Io(e)),
                }
                self.stack.push(dirs[i].clone());
            }
            Ok(self.path(dirs))
        }

        pub fn dir(&mut self, rel: &[String]) -> Result<(), Error> {
            self.enter(rel).map(|_| ())
        }

        pub fn file(&mut self, rel: &[String], _mode: Option<u32>) -> Result<NewFile, Error> {
            let (name, dirs) = rel.split_last().expect("a name");
            let dir = self.enter(dirs)?;
            for n in 1..=TRIES {
                let path = dir.join(if n == 1 { name.clone() } else { variant(name, n, true) });
                match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                    Ok(file) => return Ok(NewFile { file, path, renamed: n > 1 }),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(Error::Io(e)),
                }
            }
            Err(Error::Io(io::Error::new(io::ErrorKind::AlreadyExists, format!("every free name for “{name}” was taken"))))
        }

        /// Arbor makes no links on Windows (`plan` leaves them out); never called there.
        pub fn link(&mut self, _rel: &[String], _target: &str, _mtime: Option<SystemTime>) -> Result<bool, Error> {
            Err(Error::Unsupported("links aren't made on Windows".into()))
        }

        pub fn dir_time(&mut self, rel: &[String], t: SystemTime) -> Result<(), Error> {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_WRITE_ATTRIBUTES,
            };
            let p = self.enter(rel)?;
            let f = std::fs::OpenOptions::new()
                .access_mode(FILE_WRITE_ATTRIBUTES)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&p)?;
            f.set_modified(t)?;
            Ok(())
        }
    }

    pub(super) fn id_of(f: &File) -> io::Result<crate::noclobber::FileId> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: an open handle and a zeroed out-buffer.
        if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((u64::from(info.dwVolumeSerialNumber), (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)))
    }
}

impl Out {
    /// A file written whole: its time set, and recorded as made (by its handle).
    pub fn done(&mut self, nf: NewFile, mtime: Option<SystemTime>) -> Result<(), Error> {
        if let Some(t) = mtime {
            nf.file.set_modified(t)?;
        }
        let meta = nf.file.metadata()?;
        let id = imp::id_of(&nf.file).ok();
        self.made.push(Made { path: nf.path, dir: false, meta, id });
        Ok(())
    }

    /// Records a file that was created but not finished (a failure or a cancel midway), so the
    /// cleanup removes it.
    pub fn abandon(&mut self, nf: NewFile) {
        if let Ok(meta) = nf.file.metadata() {
            let id = imp::id_of(&nf.file).ok();
            self.made.push(Made { path: nf.path, dir: false, meta, id });
        }
    }
}
