//! Select by Pattern (NORTHSTAR Core), Invert Selection and Select All of This Kind.
//!
//! ⌘S opens a small box over the window: type a glob (`*.jpg`, `IMG_*`, `*.{png,jpg}`; see
//! `pattern.rs`) and it counts the matches live ("12 of 340 match") without touching the
//! selection. Enter replaces the selection with the matches, ⇧Enter adds them, ⌥Enter takes
//! them out. Glob only: regex would need a crate.
//!
//! It matches the rows on screen: hidden files only while they're shown, and only what the
//! filter lets through. The selection is a set of names (`Folder::marked`), so a listing that
//! refreshes keeps it. Case follows the platform's usual file system (insensitive on macOS and
//! Windows, sensitive on Linux) unless `select.case_sensitive` says otherwise.
//!
//! Arbor always has a selected row while a folder has any (with nothing marked, the cursor's
//! row is the selection), so taking out every selected item leaves the cursor on the nearest
//! row that isn't one of them.

use std::collections::HashSet;
use std::ffi::OsString;

use rune::{Key, KeyEvent, TextEdit, Tx};

use crate::app::State;
use crate::commands::Cmd;
use crate::folder::Folder;
use crate::fs;
use crate::pattern::{Case, Glob};

/// What Enter does with the matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    Replace,
    Add,
    Remove,
}

pub struct PickBox {
    pub edit: TextEdit,
    /// The live count for `counted_for`: matches and rows, or why the pattern can't be used.
    pub count: Result<(usize, usize), String>,
    /// What it was counted for: the pattern, the rows (the folder's `version`, which changes
    /// whenever they're replaced: a listing landing, hidden files shown, a filter; and how many
    /// there are, which changes when search results are appended without a new version) and
    /// the case.
    counted_for: Option<(String, (u64, usize), Case)>,
}

impl Folder {
    /// The names on screen that match, in display order.
    pub fn matching(&self, g: &Glob) -> Vec<OsString> {
        let folded = g.case() == Case::Insensitive;
        self.visible
            .iter()
            .map(|&i| &self.entries[i as usize])
            .filter(|e| g.matches_folded(if folded { &e.lower } else { &e.name }))
            .map(|e| e.os.clone())
            .collect()
    }

    /// How many rows on screen match: what the box counts on every key.
    pub fn count_matching(&self, g: &Glob) -> usize {
        let folded = g.case() == Case::Insensitive;
        self.visible
            .iter()
            .filter(|&&i| {
                let e = &self.entries[i as usize];
                g.matches_folded(if folded { &e.lower } else { &e.name })
            })
            .count()
    }

    /// The selection as names: what's marked, or the cursor's row while it's selected
    /// (nothing, with nothing selected).
    pub fn selection(&self) -> HashSet<OsString> {
        self.selection_names().0.into_iter().collect()
    }

    /// Makes `names` the selection, moving the cursor onto it if it's off it. Nothing to select
    /// can't be had (the cursor's row is the selection then), so an empty set puts the cursor on
    /// the nearest row not in `avoid` instead. False when every row is in `avoid`.
    pub fn set_selection(&mut self, names: HashSet<OsString>, avoid: &HashSet<OsString>) -> bool {
        self.picks += 1;
        self.none = false;
        fn name(f: &Folder, i: usize) -> &OsString {
            &f.entries[f.visible[i] as usize].os
        }
        if names.is_empty() {
            self.marked.clear();
            let n = self.visible.len();
            let ok = |i: usize| !avoid.contains(name(self, i));
            let near = (self.sel..n).chain((0..self.sel.min(n)).rev()).find(|&i| ok(i));
            match near {
                Some(i) => {
                    self.sel = i;
                    self.anchor = i;
                    return true;
                }
                None => return n == 0,
            }
        }
        let on = |f: &Folder, i: usize| i < f.visible.len() && names.contains(name(f, i));
        if !on(self, self.sel)
            && let Some(i) = (0..self.visible.len()).find(|&i| on(self, i))
        {
            self.sel = i;
        }
        self.anchor = self.sel;
        // The rows on screen with those names (a pattern looks at every row anyway).
        self.marked.clear();
        for r in 0..self.visible.len() {
            if names.contains(name(self, r)) {
                self.marked.set(r, true, &self.visible);
            }
        }
        true
    }

    /// Applies the matches `how` says. The new selection's size (0: none could be left).
    pub fn apply_matches(&mut self, matches: Vec<OsString>, how: How) -> usize {
        let matches: HashSet<OsString> = matches.into_iter().collect();
        let mut sel = match how {
            How::Replace => HashSet::new(),
            How::Add | How::Remove => self.selection(),
        };
        match how {
            How::Replace | How::Add => sel.extend(matches.iter().cloned()),
            How::Remove => sel.retain(|n| !matches.contains(n)),
        }
        let n = sel.len();
        self.set_selection(sel, &matches);
        n
    }

    /// Selects every row on screen that isn't selected, and unselects the rest: a word per 64
    /// rows. With nothing left (everything was selected) the cursor stays where it is.
    pub fn invert_selection(&mut self) -> usize {
        self.picks += 1;
        if self.marked.is_empty() && !self.none {
            self.marked.set(self.sel, true, &self.visible);
        }
        self.marked.invert(&self.visible);
        self.none = self.marked.is_empty();
        if !self.marked.contains(self.sel)
            && let Some(r) = self.marked.iter().next()
        {
            self.sel = r;
        }
        self.anchor = self.sel;
        self.marked.len()
    }

    /// Selects every row on screen of the cursor row's kind: folders for a folder, else files
    /// with the same extension (any case), or with none. What it selected, for the status bar.
    pub fn select_kind(&mut self) -> Option<String> {
        let e = self.selected()?;
        let (dir, ext) = (e.is_dir, fs::ext(&e.lower).to_string());
        let names: HashSet<OsString> = self
            .visible
            .iter()
            .map(|&i| &self.entries[i as usize])
            .filter(|e| if dir { e.is_dir } else { !e.is_dir && fs::ext(&e.lower) == ext })
            .map(|e| e.os.clone())
            .collect();
        let n = names.len();
        self.set_selection(names, &HashSet::new());
        let what = match (dir, ext.as_str()) {
            (true, _) => "folder",
            (false, "") => "file without an extension",
            (false, x) => return Some(format!("Selected {n} .{x} file{}", if n == 1 { "" } else { "s" })),
        };
        Some(format!("Selected {n} {what}{}", if n == 1 { "" } else { "s" }))
    }
}

impl State {
    /// The case Select by Pattern matches with.
    pub fn pick_case(&self) -> Case {
        Case::from_setting(self.settings.case_sensitive)
    }

    pub fn open_pick(&mut self, tx: &mut Tx) {
        self.close_overlays(tx);
        let mut edit = TextEdit::new("");
        edit.focused = true;
        self.pick = Some(PickBox { edit, count: Ok((0, 0)), counted_for: None });
        self.pick_refresh();
    }

    pub fn close_pick(&mut self, tx: &mut Tx) {
        if self.pick.take().is_some() {
            tx.blur();
        }
    }

    /// The field's binding (late writes after it closes go to a scratch field).
    pub fn pick_edit(&mut self) -> &mut TextEdit {
        match &mut self.pick {
            Some(p) => &mut p.edit,
            None => &mut self.palette_sink,
        }
    }

    /// Counts again if the pattern, the rows or the case changed since: never touches the
    /// selection. True when it counted.
    pub fn pick_refresh(&mut self) -> bool {
        let case = self.pick_case();
        let f = &self.panes[self.active].f;
        let Some(p) = &mut self.pick else { return false };
        let rows = (f.version, f.visible.len());
        if p.counted_for.as_ref().is_some_and(|(t, v, c)| *t == p.edit.text && *v == rows && *c == case) {
            return false;
        }
        let q = p.edit.text.trim();
        p.count =
            if q.is_empty() { Ok((0, f.visible.len())) } else { Glob::name(q, case).map(|g| (f.count_matching(&g), f.visible.len())) };
        p.counted_for = Some((p.edit.text.clone(), rows, case));
        true
    }

    /// The field's keys, before it edits: Enter, ⇧Enter and ⌥Enter apply, Esc closes.
    pub fn pick_field_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        tx.instant();
        let m = ev.mods;
        match ev.key {
            Key::Enter if !m.cmd && !m.ctrl => {
                let how = if m.alt {
                    How::Remove
                } else if m.shift {
                    How::Add
                } else {
                    How::Replace
                };
                self.pick_apply(how, tx);
            }
            Key::Escape if !m.cmd && !m.ctrl && !m.alt => self.close_pick(tx),
            _ => return false,
        }
        true
    }

    /// The window's keys while the box is open: its own chord closes it.
    pub fn pick_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        tx.instant();
        match self.keymap.find(|c| c.matches_tx(ev, tx)) {
            Some(Cmd::SelectPattern) => {
                self.close_pick(tx);
                return true;
            }
            Some(Cmd::Paste) => {
                tx.request_paste();
                return true;
            }
            _ => {}
        }
        if self.pick_field_key(ev, tx) {
            return true;
        }
        if let (Key::Text(t), Some(p)) = (&ev.key, &mut self.pick)
            && !ev.mods.cmd
            && !ev.mods.ctrl
        {
            p.edit.insert(t);
            p.edit.focused = true;
            self.pick_refresh();
        }
        true
    }

    /// Enter: the matches replace, join or leave the selection, and the box closes. With no
    /// pattern, or one that can't be used, it stays open.
    pub fn pick_apply(&mut self, how: How, tx: &mut Tx) {
        let case = self.pick_case();
        let Some(p) = &self.pick else { return };
        let q = p.edit.text.trim().to_string();
        let Ok(g) = Glob::name(&q, case) else { return };
        if q.is_empty() {
            return;
        }
        let f = self.cur_mut();
        let matches = f.matching(&g);
        let found = matches.len();
        if found == 0 && how != How::Remove {
            return;
        }
        let n = f.apply_matches(matches, how);
        f.reveal(tx);
        self.close_pick(tx);
        let s = |n: usize| if n == 1 { "" } else { "s" };
        self.note = Some(match how {
            How::Replace => format!("Selected {found} item{} matching {q}", s(found)),
            How::Add => format!("Added {found} item{} matching {q} · {n} selected", s(found)),
            How::Remove if n == 0 => format!("Unselected everything matching {q}"),
            How::Remove => format!("Unselected what matches {q} · {n} selected"),
        });
    }

    pub(crate) fn invert_selection(&mut self, tx: &mut Tx) {
        let f = self.cur_mut();
        let n = f.invert_selection();
        f.reveal(tx);
        self.note = Some(if n == 0 { "Everything was selected, so the cursor's row stays".into() } else { format!("{n} selected") });
    }

    pub(crate) fn select_kind(&mut self, tx: &mut Tx) {
        let f = self.cur_mut();
        let what = f.select_kind();
        f.reveal(tx);
        self.note = what;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Add (and Remove) work on the selection as it is: with nothing selected (the cursor
    /// ⌘-clicked off), adding a match selects the match alone, not the cursor's item too.
    #[test]
    fn adding_to_nothing_selects_only_the_matches() {
        let mut f =
            crate::folder::Folder::new(0, std::path::PathBuf::from("/"), crate::fs::Sort::new(crate::fs::SortKey::Name, false), true, None);
        f.entries = ["a", "b", "c"].iter().map(|n| crate::fs::Entry::test(n, false)).collect::<Vec<_>>().into();
        f.refilter(None);
        f.select(0);
        f.toggle_mark(0);
        assert!(f.targets().is_empty());
        f.apply_matches(vec!["b".into()], How::Add);
        assert_eq!(f.targets().iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["b"]);
    }
    use std::path::{Path, PathBuf};

    fn folder(name: &str, files: &[&str]) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-select-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for f in files {
            if let Some(dir) = f.strip_suffix('/') {
                std::fs::create_dir_all(d.join(dir)).unwrap();
            } else {
                std::fs::write(d.join(f), "x").unwrap();
            }
        }
        d
    }

    fn names(s: &State) -> Vec<String> {
        let mut v: Vec<String> = s.cur().targets().iter().map(|e| e.name.clone()).collect();
        v.sort();
        v
    }

    fn typed(s: &mut State, q: &str) {
        s.pick_edit().insert(q);
        s.pick_refresh();
    }

    fn state(d: &Path) -> State {
        crate::app::tests::state_in(d)
    }

    const FILES: &[&str] = &["a.jpg", "b.JPG", "c.png", "d.txt", "IMG_1.heic", "IMG_2.heic", ".hidden.jpg", "photos/"];

    #[test]
    fn the_live_count_never_touches_the_selection() {
        let d = folder("count", FILES);
        let mut s = state(&d);
        let mut tx = Tx::default();
        s.cur_mut().select(2);
        let before = (s.cur().sel, s.cur().marked.clone());
        s.open_pick(&mut tx);
        typed(&mut s, "*.{png,txt}");
        let visible = s.cur().visible.len();
        assert_eq!(s.pick.as_ref().unwrap().count, Ok((2, visible)));
        assert_eq!((s.cur().sel, s.cur().marked.clone()), before);
        // Hidden files count only while shown.
        s.pick_edit().clear();
        typed(&mut s, "*.jpg");
        let n = |s: &State| s.pick.as_ref().unwrap().count.clone().unwrap().0;
        let jpg = if Case::platform() == Case::Insensitive { 2 } else { 1 };
        assert_eq!(n(&s), jpg);
        s.toggle_hidden();
        s.pick.as_mut().unwrap().counted_for = None;
        s.pick_refresh();
        assert_eq!(n(&s), jpg + 1);
        assert_eq!((s.cur().sel, s.cur().marked.clone()).1, before.1);
    }

    #[test]
    fn enter_replaces_shift_adds_and_alt_removes() {
        let d = folder("apply", FILES);
        let mut s = state(&d);
        s.settings.case_sensitive = Some(false);
        let mut tx = Tx::default();
        s.open_pick(&mut tx);
        typed(&mut s, "IMG_*");
        s.pick_apply(How::Replace, &mut tx);
        assert!(s.pick.is_none());
        assert_eq!(names(&s), ["IMG_1.heic", "IMG_2.heic"]);
        assert!(s.cur().selected().unwrap().name.starts_with("IMG_"), "the cursor moves onto the selection");
        s.open_pick(&mut tx);
        typed(&mut s, "*.jpg");
        s.pick_apply(How::Add, &mut tx);
        assert_eq!(names(&s), ["IMG_1.heic", "IMG_2.heic", "a.jpg", "b.JPG"]);
        s.open_pick(&mut tx);
        typed(&mut s, "img_2*");
        s.pick_apply(How::Remove, &mut tx);
        assert_eq!(names(&s), ["IMG_1.heic", "a.jpg", "b.JPG"]);
        // Nothing matches: Enter does nothing and the box stays.
        s.open_pick(&mut tx);
        typed(&mut s, "*.zip");
        s.pick_apply(How::Replace, &mut tx);
        assert!(s.pick.is_some());
        assert_eq!(names(&s), ["IMG_1.heic", "a.jpg", "b.JPG"]);
    }

    #[test]
    fn case_follows_the_platform_unless_set() {
        let d = folder("case", FILES);
        let mut s = state(&d);
        let mut tx = Tx::default();
        s.open_pick(&mut tx);
        typed(&mut s, "*.JPG");
        let n = |s: &State| s.pick.as_ref().unwrap().count.clone().unwrap().0;
        assert_eq!(n(&s), if cfg!(any(target_os = "macos", windows)) { 2 } else { 1 });
        s.settings.case_sensitive = Some(true);
        s.pick.as_mut().unwrap().counted_for = None;
        s.pick_refresh();
        assert_eq!(n(&s), 1);
        s.settings.case_sensitive = Some(false);
        s.pick.as_mut().unwrap().counted_for = None;
        s.pick_refresh();
        assert_eq!(n(&s), 2);
    }

    #[test]
    fn the_filter_limits_what_matches() {
        let d = folder("filter", FILES);
        let mut s = state(&d);
        s.cur_mut().type_filter("a");
        let mut tx = Tx::default();
        s.open_pick(&mut tx);
        typed(&mut s, "*");
        let shown = s.cur().visible.len();
        assert!(shown < 7);
        assert_eq!(s.pick.as_ref().unwrap().count, Ok((shown, shown)));
    }

    #[test]
    fn invert_and_select_kind() {
        let d = folder("invert", FILES);
        let mut s = state(&d);
        s.settings.case_sensitive = Some(false);
        let mut tx = Tx::default();
        let all = s.cur().visible.len();
        s.cur_mut().select(0); // the folder, sorted first
        s.invert_selection(&mut tx);
        assert_eq!(s.cur().targets().len(), all - 1);
        assert!(!names(&s).contains(&"photos".to_string()));
        s.invert_selection(&mut tx);
        assert_eq!(names(&s), ["photos"]);
        // Select All of This Kind: same extension, any case.
        let a = s.cur().visible.iter().position(|&i| s.cur().entries[i as usize].name == "a.jpg").unwrap();
        s.cur_mut().select(a);
        s.select_kind(&mut tx);
        assert_eq!(names(&s), ["a.jpg", "b.JPG"]);
        assert_eq!(s.note.as_deref(), Some("Selected 2 .jpg files"));
        s.cur_mut().select(0);
        s.select_kind(&mut tx);
        assert_eq!(names(&s), ["photos"]);
    }

    /// Selected by name: a fresh read of the folder keeps it.
    #[test]
    fn the_selection_survives_a_refresh() {
        let d = folder("refresh", FILES);
        let mut s = state(&d);
        let mut tx = Tx::default();
        s.open_pick(&mut tx);
        typed(&mut s, "IMG_*");
        s.pick_apply(How::Replace, &mut tx);
        std::fs::write(d.join("0-new.txt"), "x").unwrap();
        s.reload();
        crate::app::tests::settle_all(&mut s);
        assert_eq!(names(&s), ["IMG_1.heic", "IMG_2.heic"]);
    }

    /// Taking out everything selected leaves the cursor on a row that wasn't.
    #[test]
    fn removing_everything_moves_the_cursor_off_the_matches() {
        let d = folder("remove-all", FILES);
        let mut s = state(&d);
        s.settings.case_sensitive = Some(false);
        let mut tx = Tx::default();
        s.open_pick(&mut tx);
        typed(&mut s, "*.jpg");
        s.pick_apply(How::Replace, &mut tx);
        s.open_pick(&mut tx);
        typed(&mut s, "*.jpg");
        s.pick_apply(How::Remove, &mut tx);
        assert!(s.cur().marked.is_empty());
        assert!(!s.cur().selected().unwrap().lower.ends_with(".jpg"));
    }

    /// The listing refreshes while the box is open (a file operation, a reload): the count
    /// follows the rows Enter will act on, before Enter.
    #[test]
    fn the_count_follows_a_listing_that_refreshes_while_open() {
        let d = folder("refresh-count", &["a.jpg", "notes.txt"]);
        let mut s = crate::app::tests::state_in(&d);
        let mut tx = Tx::default();
        s.run(Cmd::SelectPattern, &mut tx);
        s.pick_edit().insert("*.jpg");
        s.pick_refresh();
        assert_eq!(s.pick.as_ref().unwrap().count, Ok((1, 2)));
        std::fs::write(d.join("b.jpg"), "x").unwrap();
        s.reload();
        crate::app::tests::settle_all(&mut s);
        assert_eq!(s.pick.as_ref().unwrap().count, Ok((2, 3)), "counted again when the listing landed");
        s.pick_apply(How::Replace, &mut tx);
        assert_eq!(s.cur().marked.len(), 2);
        assert_eq!(s.note.as_deref(), Some("Selected 2 items matching *.jpg"));
    }
}
