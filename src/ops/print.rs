//! Fingerprints: what an item looked like, so undo, redo and batch rename can refuse when the
//! disk changed since ([`Print`], [`Seen`]), and say how it changed ([`Diff`]).

use super::*;

/// An item's fingerprint, taken when the batch rename's preview read its folder, so applying
/// refuses an item that changed since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen(pub(super) Print);

impl Seen {
    /// `path` as it is now (not following a link); `None` when it isn't there.
    #[cfg(test)]
    pub fn of(path: &Path) -> Option<Seen> {
        Some(Seen::with(path, &path.symlink_metadata().ok()?))
    }

    /// From metadata just read for `path`.
    pub fn with(path: &Path, m: &std::fs::Metadata) -> Seen {
        Seen(Print::of(path, m))
    }
}
/// A cheap fingerprint of what an operation left on disk, taken when it finished. Undo compares
/// it with the disk and refuses on any difference, so a wrong undo can't happen silently.
///
/// - Identity: device and inode on Unix, volume serial number and file index on Windows
///   ([`noclobber::file_id`]), so a file deleted and recreated under the same name reads as
///   replaced.
/// - Type, size (files and links) and modified time. A folder's own mtime changes when an
///   entry directly inside it is added, removed or renamed.
/// - `tree`: for folders whose undo trashes them (copies, duplicates, new folders), a sum over
///   everything inside. A folder's mtime doesn't see an edit deeper down, and trashing a copy the
///   user has since worked in is exactly the wrong undo this exists to stop. Moves and renames
///   don't walk: moving a folder back carries whatever is inside with it and loses nothing, so a
///   walk would cost O(tree) to protect nothing.
///
/// Known limits (contents are never hashed: far too slow for big files):
///
/// - An edit that keeps the size and leaves the mtime where it was isn't seen. That includes an
///   edit made within the disk's timestamp granularity of the fingerprint: 2 s on FAT, 1 s on
///   HFS+ and ext3, 100 ns on NTFS, 1 ns on APFS and ext4 (but ext4 and NTFS stamp from a clock
///   that ticks every few ms, so two writes in the same tick look alike). Pinned by the test
///   `same_size_edit_within_mtime_granularity_is_not_seen`.
/// - The instant between the last check and the act. Each step is checked right before it runs,
///   and moves can't overwrite whatever appears in that instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Print {
    pub(super) kind: Kind,
    pub(super) size: u64,
    pub(super) mtime: Option<SystemTime>,
    pub(super) id: Option<(u64, u64)>,
    pub(super) tree: Option<Tree>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Kind {
    File,
    Folder,
    Link,
}

impl Kind {
    pub(super) fn of(m: &Metadata) -> Kind {
        let ft = m.file_type();
        if ft.is_symlink() {
            Kind::Link
        } else if ft.is_dir() {
            Kind::Folder
        } else {
            Kind::File
        }
    }
    pub(super) fn name(self) -> &'static str {
        match self {
            Kind::File => "a file",
            Kind::Folder => "a folder",
            Kind::Link => "a link",
        }
    }
}

impl Print {
    pub(super) fn of(path: &Path, m: &Metadata) -> Print {
        Print::with_id(m, noclobber::file_id(path, m))
    }

    /// From metadata and an identity read off a handle (`noclobber::Made`), not by name.
    pub(super) fn with_id(m: &Metadata, id: Option<(u64, u64)>) -> Print {
        let kind = Kind::of(m);
        Print { kind, size: if kind == Kind::Folder { 0 } else { m.len() }, mtime: m.modified().ok(), id, tree: None }
    }
}

/// How an item differs from its fingerprint.
pub(super) enum Diff {
    Gone,
    Kind(Kind),
    Replaced,
    Edited,
    Inside(std::cmp::Ordering),
    Unreadable,
    /// It can't be looked at (its folder can't be read): not known to be gone.
    Unreachable(io::Error),
}

impl Diff {
    /// "“name” was edited", for an item that was `was`.
    pub(super) fn say(&self, name: &str, was: Kind) -> String {
        match self {
            Diff::Gone => format!("{name} was moved or deleted"),
            Diff::Kind(k) => format!("{name} was replaced by {}", k.name()),
            Diff::Replaced => format!("{name} was replaced by another item with the same name"),
            Diff::Edited if was == Kind::Folder => format!("items were added, removed or renamed in {name}"),
            Diff::Edited => format!("{name} was edited"),
            Diff::Inside(std::cmp::Ordering::Greater) => format!("items were added inside {name}"),
            Diff::Inside(std::cmp::Ordering::Less) => format!("items were removed from inside {name}"),
            Diff::Inside(std::cmp::Ordering::Equal) => format!("something inside {name} was edited"),
            Diff::Unreadable => format!("something inside {name} can't be read any more"),
            Diff::Unreachable(e) => format!("{name} can't be checked ({e})"),
        }
    }
}

/// Everything inside a folder, in one order-independent sum: each entry's path below the folder,
/// type, size, mtime and identity, hashed and added up. Counts are kept to say what changed and
/// to stop a re-check early once it has seen more than there were.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Tree {
    pub(super) items: u64,
    pub(super) bytes: u64,
    pub(super) sum: u64,
}

impl Tree {
    /// No limit, for `walk`.
    pub(super) const ALL: Tree = Tree { items: u64::MAX, bytes: 0, sum: 0 };

    pub(super) fn add(&mut self, rel: &Path, p: &Print) {
        let mut h = std::hash::DefaultHasher::new();
        (rel, p.kind, p.size, p.mtime, p.id).hash(&mut h);
        self.items += 1;
        self.bytes += p.size;
        self.sum = self.sum.wrapping_add(h.finish());
    }
}
/// A fingerprint nothing on disk matches.
pub(super) fn unmatchable() -> Print {
    Print { kind: Kind::Link, size: u64::MAX, mtime: None, id: None, tree: None }
}
