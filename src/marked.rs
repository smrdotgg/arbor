//! A folder's multiple selection ([`Marked`]): the selected items by name, each with its size
//! and whether it's a folder, and the totals the status bar shows ("3 selected · 4.2 MB + 2
//! folders"), kept up to date by every change. A click, ⌘-click, an arrow or a selection
//! rectangle moving costs what it changes, never the whole list: the totals are worked out from
//! scratch only when the selection is made from a listing (on the listing's worker after a
//! reload, `folder::picked_in`).

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};

use crate::fs::Entry;

/// What two or more selected items hold: how many, the files' sizes added up, and how many are
/// folders (whose size isn't known: nothing walks them).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Picked {
    pub count: usize,
    pub bytes: u64,
    pub folders: usize,
}

/// The selected items, by name (`Entry::os`), with their totals.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Marked {
    /// Each item's size (0 for a folder) and whether it's a folder.
    items: HashMap<OsString, (u64, bool)>,
    bytes: u64,
    folders: usize,
}

impl Marked {
    /// The items of `entries`, selected.
    pub fn of<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Marked {
        let mut m = Marked::default();
        for e in entries {
            m.insert(e);
        }
        m
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn contains(&self, name: &OsStr) -> bool {
        self.items.contains_key(name)
    }

    /// The selected items' names, in no order.
    pub fn iter(&self) -> impl Iterator<Item = &OsString> {
        self.items.keys()
    }

    /// The totals, in no time.
    pub fn picked(&self) -> Picked {
        Picked { count: self.items.len(), bytes: self.bytes, folders: self.folders }
    }

    /// Selects `e`. True when it wasn't selected.
    pub fn insert(&mut self, e: &Entry) -> bool {
        self.insert_as(e.os.clone(), if e.is_dir { 0 } else { e.size }, e.is_dir)
    }

    /// Selects the item `name`, `size` bytes (a folder: `dir`). True when it wasn't selected.
    pub fn insert_as(&mut self, name: OsString, size: u64, dir: bool) -> bool {
        let old = self.items.insert(name, (size, dir));
        self.count(old, -1);
        self.count(Some((size, dir)), 1);
        old.is_none()
    }

    /// Deselects `name`. True when it was selected.
    pub fn remove(&mut self, name: &OsStr) -> bool {
        let old = self.items.remove(name);
        self.count(old, -1);
        old.is_some()
    }

    /// The item `old` is called `new` now (renamed): still selected, as big as it was.
    pub fn rename(&mut self, old: &OsStr, new: OsString) {
        if let Some((size, dir)) = self.items.remove(old) {
            self.count(Some((size, dir)), -1);
            self.insert_as(new, size, dir);
        }
    }

    /// Keeps the items `keep` says.
    pub fn retain(&mut self, mut keep: impl FnMut(&OsString) -> bool) {
        let (mut bytes, mut folders) = (0, 0);
        self.items.retain(|n, &mut (size, dir)| {
            let k = keep(n);
            if k {
                bytes += size;
                folders += usize::from(dir);
            }
            k
        });
        (self.bytes, self.folders) = (bytes, folders);
    }

    pub fn clear(&mut self) {
        *self = Marked::default();
    }

    /// Adds (`by` 1) or takes away (-1) an item's share of the totals.
    fn count(&mut self, item: Option<(u64, bool)>, by: i8) {
        let Some((size, dir)) = item else { return };
        if by > 0 {
            self.bytes += size;
            self.folders += usize::from(dir);
        } else {
            self.bytes -= size;
            self.folders -= usize::from(dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(name: &str, size: u64, dir: bool) -> Entry {
        Entry { size, ..Entry::test(name, dir) }
    }

    /// Any run of selects, deselects, renames and keeps leaves the totals what adding up the
    /// selection from scratch gives.
    #[test]
    fn the_totals_follow_every_change() {
        let pool: Vec<Entry> = (0..12).map(|i| e(&format!("n{i}"), i * 1000 + 7, i % 4 == 0)).collect();
        crate::prop::check("marked_totals", 500, |r| {
            let mut m = Marked::default();
            for _ in 0..r.below(40) {
                let x = &pool[r.below(pool.len())];
                match r.below(5) {
                    0 | 1 => {
                        m.insert(x);
                    }
                    2 => {
                        m.remove(&x.os);
                    }
                    3 => {
                        let to = OsString::from(format!("{}x", x.name));
                        m.rename(&x.os, to);
                    }
                    _ => {
                        let n = r.below(12);
                        m.retain(|name| name.len() % 3 != n % 3);
                    }
                }
                let (bytes, folders) = m.items.values().fold((0, 0), |(b, f), &(s, d)| (b + s, f + usize::from(d)));
                assert_eq!(m.picked(), Picked { count: m.items.len(), bytes, folders });
            }
        });
    }
}
