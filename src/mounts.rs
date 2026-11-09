//! Where one disk ends and another begins, for every walk that must not cross into another
//! file system: permanent delete (`delete.rs`), the cleanup of a cancelled copy and of a
//! cross-disk move's originals (`ops`), and Get Info's total (`info.rs`).
//!
//! - A folder on another device than the walk's root is another disk.
//! - Linux: a mount point on the same device (a bind mount) is one too, from
//!   `/proc/self/mountinfo`, read once per operation ([`Mounts`]).
//! - macOS: the APFS volumes of one group share a device number, and firmlinks (`/Users`,
//!   `/Applications`) lead from one to the other, so each folder's volume is compared as well
//!   ([`volume`]).
//!
//! Search goes by device numbers alone (`search::devices_for`), so a bind mount on the same disk
//! is walked there (and its folders' identities keep it from being walked twice).

use std::path::{Path, PathBuf};

/// Where other file systems are mounted: read once per operation, on its worker. Only Linux
/// lists them (for bind mounts, which share their device); elsewhere every mount has a device
/// of its own, which the walk compares.
#[derive(Clone, Debug, Default)]
pub struct Mounts(Vec<PathBuf>);

impl Mounts {
    pub fn read() -> Mounts {
        #[cfg(target_os = "linux")]
        if let Ok(s) = std::fs::read_to_string("/proc/self/mountinfo") {
            return Mounts::parse(&s);
        }
        Mounts::default()
    }

    /// `/proc/self/mountinfo`'s mount points (the fifth field, with its octal escapes).
    #[cfg(any(target_os = "linux", test))]
    pub fn parse(s: &str) -> Mounts {
        Mounts(s.lines().filter_map(|l| l.split(' ').nth(4)).map(unescape).collect())
    }

    /// Exactly these mount points (tests).
    #[cfg(test)]
    pub fn of(points: &[&Path]) -> Mounts {
        Mounts(points.iter().map(|p| p.to_path_buf()).collect())
    }

    /// The mount points at or below `root` (a canonical path).
    pub(crate) fn under(&self, root: &Path) -> Vec<PathBuf> {
        self.0.iter().filter(|m| m.starts_with(root)).cloned().collect()
    }
}

/// `\040` and the like back to their bytes: `/proc/self/mounts` and `mountinfo` write a space
/// as `\040`, a tab `\011`, a newline `\012` and a backslash `\134`.
#[cfg_attr(not(any(test, all(unix, not(target_os = "macos")))), allow(dead_code))]
pub fn unescape_bytes(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// A mount point from the mount table, as a path.
#[cfg(any(target_os = "linux", test))]
fn unescape(s: &str) -> PathBuf {
    let out = unescape_bytes(s);
    #[cfg(unix)]
    return PathBuf::from(<std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(out));
    #[cfg(not(unix))]
    PathBuf::from(String::from_utf8_lossy(&out).into_owned())
}

/// macOS: which volume a folder is on, as the mount point `statfs` names (`f_mntonname`). The
/// APFS volumes of one group share a device number, and a firmlink (`/Users`, `/Applications`)
/// leads from the system volume to the Data volume, so a device number alone doesn't tell
/// them apart. Elsewhere `None`: the device number does.
pub mod volume {
    #[cfg(target_os = "macos")]
    fn name(s: &libc::statfs) -> Vec<u8> {
        s.f_mntonname.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect()
    }

    /// The volume of the open folder `fd`.
    #[cfg(unix)]
    pub fn of_fd(fd: std::os::fd::RawFd) -> Option<Vec<u8>> {
        #[cfg(target_os = "macos")]
        {
            // SAFETY: a zeroed statfs is a valid out-buffer for an open descriptor.
            let mut s: libc::statfs = unsafe { std::mem::zeroed() };
            (unsafe { libc::fstatfs(fd, &mut s) } == 0).then(|| name(&s))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = fd;
            None
        }
    }

    /// The volume of the folder at `p` (following links: call it for folders).
    pub fn of_path(p: &std::path::Path) -> Option<Vec<u8>> {
        #[cfg(target_os = "macos")]
        {
            let c = crate::ffi::cstr(p).ok()?;
            // SAFETY: a NUL-terminated path and a zeroed out-buffer.
            let mut s: libc::statfs = unsafe { std::mem::zeroed() };
            (unsafe { libc::statfs(c.as_ptr(), &mut s) } == 0).then(|| name(&s))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = p;
            None
        }
    }
}

/// The mount boundaries of one tree: its device (and on macOS its volume), and the mount points
/// inside it (by canonical path). For the other recursive removals (a cancelled copy's leftovers,
/// a cross-disk move's originals) and the count of what's left, which go by path.
pub struct Fence {
    root: PathBuf,
    canon: PathBuf,
    #[cfg(unix)]
    dev: Option<u64>,
    vol: Option<Vec<u8>>,
    inside: Vec<PathBuf>,
}

impl Fence {
    pub fn new(root: &Path, mounts: &Mounts) -> Fence {
        let canon = canonical(root);
        let meta = root.symlink_metadata().ok();
        #[cfg(unix)]
        let dev = meta.as_ref().map(std::os::unix::fs::MetadataExt::dev);
        Fence {
            root: root.to_path_buf(),
            inside: mounts.under(&canon),
            canon,
            #[cfg(unix)]
            dev,
            vol: meta.filter(|m| m.is_dir()).and_then(|_| volume::of_path(root)),
        }
    }

    /// Whether `p` (in the tree, with its own `lstat`) is a mount point or on another disk.
    pub fn crosses(&self, p: &Path, m: &std::fs::Metadata) -> bool {
        #[cfg(unix)]
        if self.dev.is_some_and(|d| d != std::os::unix::fs::MetadataExt::dev(m)) {
            return true;
        }
        if m.is_dir() && self.vol.is_some() && volume::of_path(p) != self.vol {
            return true;
        }
        #[cfg(not(unix))]
        let _ = m;
        let rel = p.strip_prefix(&self.root).unwrap_or(p);
        !self.inside.is_empty() && self.inside.iter().any(|i| self.canon.join(rel).starts_with(i))
    }
}

/// `p` with its parent's links resolved (its own name kept as it is, since it may be a link).
pub(crate) fn canonical(p: &Path) -> PathBuf {
    match (p.parent(), p.file_name()) {
        (Some(d), Some(n)) => {
            std::fs::canonicalize(if d.as_os_str().is_empty() { Path::new(".") } else { d }).map_or_else(|_| p.into(), |d| d.join(n))
        }
        _ => p.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_mount_table() {
        let m = Mounts::parse(
            "22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/root rw\n\
             36 22 0:31 / /media/my\\040disk rw,nosuid - fuse.sshfs host: rw\n\
             37 22 0:32 /sub /srv/bind rw - ext4 /dev/root rw\n",
        );
        assert_eq!(m.0, [PathBuf::from("/"), PathBuf::from("/media/my disk"), PathBuf::from("/srv/bind")]);
        assert_eq!(m.under(Path::new("/media")), [PathBuf::from("/media/my disk")]);
        assert!(m.under(Path::new("/srv/bin")).is_empty(), "a path prefix isn't a parent");
    }
}
