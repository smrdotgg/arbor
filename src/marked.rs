//! A folder's rows on screen ([`Rows`]) and its multiple selection ([`Marked`]), built so that
//! every change to the selection costs what it changes, at most a word per 64 rows, however
//! big the folder: a click, ⌘-click, ⇧-arrow growing or shrinking a range, ⌘A, Invert, Esc,
//! a selection rectangle's move.
//!
//! - **The selection is a set of rows on screen** (bits over [`Rows`]), so what's selected is
//!   exactly what an action acts on and what the status bar adds up: an item hidden (hidden
//!   files turned off, a filter) isn't selected any more.
//! - **Totals** (count, the files' bytes, folders) are kept with every change; a run of rows is
//!   added up from the rows' running sums ([`Rows`] keeps them, built with the rows, which look
//!   at every entry anyway).
//! - **When the rows are made again** (a listing lands, a filter, hidden files, a sort), the
//!   selection is carried across on its entries (`Entry::sel`, set for the selected rows just
//!   before and read as the new rows are built); a reload's selection by name is marked on its
//!   entries on the listing's worker.

use std::ops::Range;

use crate::fs::Entry;

/// What two or more selected items hold: how many, the files' sizes added up, and how many are
/// folders (whose size isn't known: nothing walks them).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Picked {
    pub count: usize,
    pub bytes: u64,
    pub folders: usize,
}

/// The rows on screen: each one's entry (an index into the folder's entries), and running sums
/// of the files' sizes and of folders, so any run of rows adds up at once.
#[derive(Clone, Debug, PartialEq)]
pub struct Rows {
    idx: Vec<u32>,
    /// `bytes[r]`: the files' sizes in rows before `r` (one more than there are rows).
    bytes: Vec<u64>,
    /// `dirs[r]`: the folders in rows before `r`.
    dirs: Vec<u32>,
}

impl Default for Rows {
    fn default() -> Rows {
        Rows { idx: Vec::new(), bytes: vec![0], dirs: vec![0] }
    }
}

impl std::ops::Deref for Rows {
    type Target = [u32];
    fn deref(&self) -> &[u32] {
        &self.idx
    }
}

impl Rows {
    /// The rows for `entries[range]` that `keep` keeps, in order.
    pub fn of(entries: &[Entry], range: Range<usize>, keep: impl FnMut(&Entry) -> bool) -> Rows {
        let mut r = Rows::default();
        r.extend(entries, range, keep);
        r
    }

    /// Adds rows for `entries[range]` that `keep` keeps, after the others.
    pub fn extend(&mut self, entries: &[Entry], range: Range<usize>, mut keep: impl FnMut(&Entry) -> bool) {
        let (mut bytes, mut dirs) = (*self.bytes.last().unwrap_or(&0), *self.dirs.last().unwrap_or(&0));
        for i in range {
            let e = &entries[i];
            if keep(e) {
                self.idx.push(i as u32);
                if e.is_dir {
                    dirs += 1;
                } else {
                    bytes += e.size;
                }
                self.bytes.push(bytes);
                self.dirs.push(dirs);
            }
        }
    }

    /// Row `r`'s size (0 for a folder) and whether it's a folder.
    fn row(&self, r: usize) -> (u64, bool) {
        (self.bytes[r + 1] - self.bytes[r], self.dirs[r + 1] > self.dirs[r])
    }

    /// The files' bytes and the folders in rows `a..b`.
    fn span(&self, a: usize, b: usize) -> (u64, usize) {
        (self.bytes[b] - self.bytes[a], (self.dirs[b] - self.dirs[a]) as usize)
    }
}

/// The selected rows (of a [`Rows`]), with their totals.
#[derive(Clone, Debug, Default)]
pub struct Marked {
    bits: Vec<u64>,
    count: usize,
    bytes: u64,
    folders: usize,
    /// The selection is exactly rows `a..=b` (a ⇧ range): growing or shrinking it changes only
    /// the rows between the old end and the new.
    range: Option<(usize, usize)>,
}

impl Marked {
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn contains(&self, row: usize) -> bool {
        self.bits.get(row / 64).is_some_and(|w| w & (1 << (row % 64)) != 0)
    }

    /// The selected rows, in order: a word per 64 rows, then one step per selected row.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.bits.iter().enumerate().flat_map(|(i, &w)| {
            let mut w = w;
            std::iter::from_fn(move || {
                if w == 0 {
                    return None;
                }
                let b = w.trailing_zeros() as usize;
                w &= w - 1;
                Some(i * 64 + b)
            })
        })
    }

    /// The totals, in no time.
    pub fn picked(&self) -> Picked {
        Picked { count: self.count, bytes: self.bytes, folders: self.folders }
    }

    /// Selects (`on`) or deselects row `row` of `rows`. True when that changed it.
    pub fn set(&mut self, row: usize, on: bool, rows: &Rows) -> bool {
        if row >= rows.len() || self.contains(row) == on {
            return false;
        }
        let w = row / 64;
        if self.bits.len() <= w {
            self.bits.resize(w + 1, 0);
        }
        self.bits[w] ^= 1 << (row % 64);
        let (size, dir) = rows.row(row);
        if on {
            self.count += 1;
            self.bytes += size;
            self.folders += usize::from(dir);
        } else {
            self.count -= 1;
            self.bytes -= size;
            self.folders -= usize::from(dir);
        }
        self.range = None;
        true
    }

    /// Selects row `row` if it isn't, else deselects it.
    pub fn toggle(&mut self, row: usize, rows: &Rows) {
        let on = !self.contains(row);
        self.set(row, on, rows);
    }

    /// Nothing selected: a word per 64 rows.
    pub fn clear(&mut self) {
        self.bits.fill(0);
        (self.count, self.bytes, self.folders, self.range) = (0, 0, 0, None);
    }

    /// The selection becomes exactly rows `a..=b` of `rows` (within them). Grown or shrunk from
    /// the range it was, only the rows between change.
    pub fn select_range(&mut self, a: usize, b: usize, rows: &Rows) {
        let n = rows.len();
        if n == 0 {
            self.clear();
            return;
        }
        let (a, b) = (a.min(b).min(n - 1), a.max(b).min(n - 1));
        if self.bits.len() < n.div_ceil(64) {
            self.bits.resize(n.div_ceil(64), 0);
        }
        match self.range {
            Some((c, d)) => {
                // Off what's no longer in it, on what's new: the rows between the two ranges.
                for (lo, hi, on) in
                    [(c, a.min(d + 1), false), (c.max(b + 1), d + 1, false), (a, c.min(b + 1), true), (a.max(d + 1), b + 1, true)]
                {
                    if lo < hi {
                        fill(&mut self.bits, lo..hi, on);
                    }
                }
            }
            None => {
                self.bits.fill(0);
                fill(&mut self.bits, a..b + 1, true);
            }
        }
        let (bytes, folders) = rows.span(a, b + 1);
        (self.count, self.bytes, self.folders, self.range) = (b + 1 - a, bytes, folders, Some((a, b)));
    }

    /// Every row of `rows` that isn't selected, and none of those that are.
    pub fn invert(&mut self, rows: &Rows) {
        let n = rows.len();
        if self.bits.len() < n.div_ceil(64) {
            self.bits.resize(n.div_ceil(64), 0);
        }
        for (i, w) in self.bits.iter_mut().enumerate() {
            let lo = i * 64;
            let mask = if lo + 64 <= n {
                u64::MAX
            } else if lo < n {
                (1 << (n - lo)) - 1
            } else {
                0
            };
            *w = (*w ^ mask) & mask;
        }
        let (bytes, folders) = rows.span(0, n);
        (self.count, self.bytes, self.folders) = (n - self.count, bytes - self.bytes, folders - self.folders);
        self.range = None;
    }
}

/// The same rows selected (however they were selected).
impl PartialEq for Marked {
    fn eq(&self, o: &Marked) -> bool {
        self.count == o.count && self.iter().eq(o.iter())
    }
}

/// A plain set of indices into a listing's entries: the selection carried by identity while
/// the rows are made again (a filter, a sort, which moves it with the entries, a reload, whose
/// worker marks it on the new listing).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bits(Vec<u64>);

impl Bits {
    pub fn get(&self, i: usize) -> bool {
        self.0.get(i / 64).is_some_and(|w| w & (1 << (i % 64)) != 0)
    }

    pub fn set(&mut self, i: usize) {
        if self.0.len() <= i / 64 {
            self.0.resize(i / 64 + 1, 0);
        }
        self.0[i / 64] |= 1 << (i % 64);
    }

    pub fn count(&self) -> usize {
        self.0.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// The highest index in it.
    pub fn last(&self) -> Option<usize> {
        self.0.iter().rposition(|&w| w != 0).map(|i| i * 64 + 63 - self.0[i].leading_zeros() as usize)
    }

    /// The same items after their entries moved: `perm[j]` is where entry `j` came from.
    pub fn permuted(&self, perm: &[u32]) -> Bits {
        let mut b = Bits::default();
        for (j, &from) in perm.iter().enumerate() {
            if self.get(from as usize) {
                b.set(j);
            }
        }
        b
    }
}

/// Sets (`on`) or clears the bits `r` in `bits`, a word at a time.
fn fill(bits: &mut [u64], r: Range<usize>, on: bool) {
    let mut i = r.start;
    while i < r.end {
        let (w, b) = (i / 64, i % 64);
        let len = (64 - b).min(r.end - i);
        let mask = if len == 64 { u64::MAX } else { ((1u64 << len) - 1) << b };
        if on {
            bits[w] |= mask;
        } else {
            bits[w] &= !mask;
        }
        i += len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: usize) -> (Vec<Entry>, Rows) {
        let entries: Vec<Entry> = (0..n)
            .map(|i| Entry { size: if i % 7 == 3 { 0 } else { i as u64 * 10 + 1 }, ..Entry::test(&format!("n{i}"), i % 7 == 3) })
            .collect();
        let r = Rows::of(&entries, 0..n, |e| !e.name.ends_with('5'));
        (entries, r)
    }

    /// Any run of clicks, ⌘-clicks, ⇧ ranges, inversions and clears leaves exactly the rows a
    /// plain set would have, with the totals adding them up from scratch gives.
    #[test]
    fn every_change_matches_a_plain_set() {
        let (entries, r) = rows(300);
        let n = r.len();
        crate::prop::check("marked_rows", 400, |g| {
            let mut m = Marked::default();
            let mut want = std::collections::BTreeSet::new();
            for _ in 0..g.below(30) {
                match g.below(6) {
                    0 | 1 => {
                        let x = g.below(n + 3);
                        m.toggle(x, &r);
                        if x < n && !want.remove(&x) {
                            want.insert(x);
                        }
                    }
                    2 | 3 => {
                        let (a, b) = (g.below(n), g.below(n));
                        m.select_range(a, b, &r);
                        want = (a.min(b)..=a.max(b)).collect();
                    }
                    4 => {
                        m.invert(&r);
                        want = (0..n).filter(|x| !want.contains(x)).collect();
                    }
                    _ => {
                        m.clear();
                        want.clear();
                    }
                }
                assert_eq!(m.iter().collect::<Vec<_>>(), want.iter().copied().collect::<Vec<_>>());
                let (bytes, folders) = want.iter().fold((0, 0), |(b, f), &x| {
                    let e = &entries[r[x] as usize];
                    if e.is_dir { (b, f + 1) } else { (b + e.size, f) }
                });
                assert_eq!(m.picked(), Picked { count: want.len(), bytes, folders });
            }
        });
    }

    #[test]
    fn running_sums_add_up_any_run_of_rows() {
        let (entries, r) = rows(200);
        for (a, b) in [(0, 0), (0, r.len()), (17, 90), (r.len() - 1, r.len())] {
            let (bytes, dirs) = r.span(a, b);
            let want = r[a..b].iter().map(|&i| &entries[i as usize]);
            assert_eq!(bytes, want.clone().filter(|e| !e.is_dir).map(|e| e.size).sum::<u64>());
            assert_eq!(dirs, want.filter(|e| e.is_dir).count());
        }
    }
}
