//! Tabs and split view. The window has one or two panes side by side; each has its own tabs,
//! and each tab its own folder, history, selection, scroll, filter and sort. One pane is
//! active: it gets keys, commands and the status bar. File operations stay global (one
//! operation at a time, one undo stack, one clipboard).
//!
//! Only the visible tab of each pane has a listing (`Pane::f`). A hidden tab is a
//! [`TabMemo`]: no entries and no work, no reads and no watching. Showing it reads its folder
//! fresh, waiting at most [`crate::app::FIRST_PAINT_WAIT`] so a local folder lands in the
//! same frame, then restores the selection by name and the scroll.

use rune::Tx;

use crate::app::{NOT_SPLIT, Pane, State, title};
use crate::folder::{Folder, TabMemo};
use crate::ops::Op;

/// How many closed tabs "Reopen Closed Tab" remembers.
const CLOSED_MAX: usize = 20;

impl State {
    /// The pane that isn't active, when split.
    pub fn other(&self) -> Option<&Folder> {
        self.panes.get(1 - self.active.min(1)).filter(|_| self.split()).map(|p| &p.f)
    }

    /// Makes pane `ix` the active one: it gets keys and the status bar. A rename in the other
    /// pane ends (unsaved, as Esc would).
    pub fn activate(&mut self, ix: usize, tx: &mut Tx) {
        if ix == self.active || ix >= self.panes.len() {
            return;
        }
        if self.rename.take().is_some() {
            tx.blur();
        }
        self.active = ix;
        tx.set_title(title(&self.cur().cwd));
        self.sync_preview();
    }

    /// A click in the pane with this id. False when it has closed since the frame drawn.
    pub fn activate_id(&mut self, id: u64, tx: &mut Tx) -> bool {
        match self.pane_ix(id) {
            Some(ix) => {
                self.activate(ix, tx);
                true
            }
            None => false,
        }
    }

    /// ⌘T: a new tab on the same folder, after this one. The listing on screen becomes the new
    /// tab's (no read); the old tab keeps only its lightweight state.
    pub fn new_tab(&mut self, tx: &mut Tx) {
        if self.rename.take().is_some() {
            tx.blur();
        }
        let p = &mut self.panes[self.active];
        p.tabs[p.tab] = Some(p.f.memo());
        p.tab += 1;
        p.tabs.insert(p.tab, None);
        p.f.back.clear();
        p.f.fwd.clear();
        if !p.f.filter.is_empty() {
            p.f.filter.clear();
            let keep = p.f.cursor_name();
            p.f.refilter(keep.as_deref());
        }
    }

    /// Shows tab `j` of pane `ix`, reading its folder fresh (see the module docs).
    pub fn show_tab(&mut self, ix: usize, j: usize, tx: &mut Tx) {
        let p = &mut self.panes[ix];
        if j == p.tab || j >= p.tabs.len() {
            return;
        }
        let memo = p.f.memo();
        p.tabs[p.tab] = Some(memo);
        let m = p.tabs[j].take().expect("a hidden tab has its memo");
        p.tab = j;
        self.reveal_tab(ix, m, tx);
    }

    /// Puts `m` on screen in pane `ix` (whose tab slot for it is already `None`).
    fn reveal_tab(&mut self, ix: usize, m: TabMemo, tx: &mut Tx) {
        if ix == self.active && self.rename.take().is_some() {
            tx.blur();
        }
        let wait = self.show_wait;
        let f = &mut self.panes[ix].f;
        f.show(m);
        if ix == self.active {
            tx.set_title(title(&f.cwd));
        }
        if let Some(listing) = f.request_now(wait) {
            self.land(ix, listing, tx);
        }
        self.sync_preview();
    }

    /// ⌃Tab / ⌃⇧Tab: the next or previous tab of the active pane, wrapping around.
    pub fn cycle_tab(&mut self, by: isize, tx: &mut Tx) {
        let p = &self.panes[self.active];
        let n = p.tabs.len() as isize;
        let j = (p.tab as isize + by).rem_euclid(n) as usize;
        self.show_tab(self.active, j, tx);
    }

    /// ⌘W (or a middle click on a tab): closes tab `j` of pane `ix`. Its lightweight state goes
    /// to the closed list. The last tab of a pane closes the pane when split; the last tab of
    /// the last pane stays (see `unavailable`).
    pub fn close_tab(&mut self, ix: usize, j: usize, tx: &mut Tx) {
        let Some(p) = self.panes.get(ix) else { return };
        if j >= p.tabs.len() {
            return;
        }
        if p.tabs.len() == 1 {
            if self.split() {
                self.close_pane(ix, tx);
            }
            return;
        }
        let p = &mut self.panes[ix];
        let memo = match p.tabs.remove(j) {
            Some(m) => m,
            None => p.f.memo(),
        };
        self.remember_closed(memo);
        let p = &mut self.panes[ix];
        if j < p.tab {
            p.tab -= 1;
        } else if j == p.tab {
            let next = j.min(p.tabs.len() - 1);
            p.tab = next;
            let m = p.tabs[next].take().expect("a hidden tab has its memo");
            self.reveal_tab(ix, m, tx);
        }
    }

    /// Closes pane `ix` and its tabs (each can be reopened). The other pane stays, active.
    fn close_pane(&mut self, ix: usize, tx: &mut Tx) {
        if !self.split() {
            return;
        }
        let Pane { f, tabs, tab } = self.panes.remove(ix);
        let live = f.memo();
        for (i, m) in tabs.into_iter().enumerate() {
            self.remember_closed(if i == tab { live.clone() } else { m.unwrap_or_else(|| live.clone()) });
        }
        if self.rename.take().is_some() {
            tx.blur();
        }
        self.pane_drop = None;
        self.active = 0;
        tx.set_title(title(&self.cur().cwd));
        self.sync_preview();
    }

    fn remember_closed(&mut self, m: TabMemo) {
        if self.closed.len() == CLOSED_MAX {
            self.closed.remove(0);
        }
        self.closed.push(m);
    }

    /// ⇧⌘T: the last closed tab, as a new tab after the active one, read fresh.
    pub fn reopen_tab(&mut self, tx: &mut Tx) {
        let Some(m) = self.closed.pop() else { return };
        let ix = self.active;
        let p = &mut self.panes[ix];
        p.tabs[p.tab] = Some(p.f.memo());
        p.tab += 1;
        p.tabs.insert(p.tab, None);
        self.reveal_tab(ix, m, tx);
    }

    /// ⌘\: splits the window, the new pane on the active pane's folder (and active), or closes
    /// the other pane, keeping the active one.
    pub fn toggle_split(&mut self, tx: &mut Tx) {
        if self.split() {
            self.close_pane(1 - self.active, tx);
            return;
        }
        let id = self.new_pane_id();
        let f = Folder::same_as(id, self.cur(), self.waker.clone());
        self.panes.push(Pane { f, tabs: vec![None], tab: 0 });
        self.activate(1, tx);
    }

    /// F5 / F6: copies or moves the selection into the other pane's folder, as a paste would
    /// (on a worker, never over anything, undoable).
    pub fn send_to_other_pane(&mut self, mv: bool) {
        let Some(dest) = self.other().map(|o| o.cwd.clone()) else {
            self.note = Some(NOT_SPLIT.into());
            return;
        };
        let sources = self.target_paths();
        if sources.is_empty() {
            return;
        }
        self.start(if mv { Op::Move { sources, dest } } else { Op::Copy { sources, dest } });
    }

    /// A row drag from pane `from` (row `i`, drawn at `version`) let go over pane `to`: into
    /// its folder, as a file drop (same disk moves, another copies, ⌥ copies).
    pub fn drop_on_pane(&mut self, from: u64, i: usize, version: u64, to: u64, tx: &mut Tx) {
        self.pane_drop = None;
        let (Some(src), Some(dst)) = (self.pane(from), self.pane(to)) else { return };
        if src.f.version != version || from == to {
            return;
        }
        let (files, into) = (src.f.drag_paths(i), dst.f.cwd.clone());
        self.drop_files(&files, into, tx);
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use rune::Tx;

    use crate::app::State;
    use crate::app::tests::state_in;
    use crate::commands::Cmd;

    fn folder(name: &str, files: &[&str]) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-panes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for f in files {
            std::fs::write(d.join(f), "x").unwrap();
        }
        d
    }

    /// Polls as frames would until no pane is loading (or 10 s pass).
    fn settle(s: &mut State) {
        let until = Instant::now() + Duration::from_secs(10);
        while s.panes.iter().any(|p| p.f.is_loading()) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
            s.poll(&mut Tx::default());
        }
        assert!(!s.panes.iter().any(|p| p.f.is_loading()), "never landed");
    }

    /// Polls as frames would until the file operation is done and no pane is loading.
    fn finish(s: &mut State) {
        let until = Instant::now() + Duration::from_secs(10);
        while s.job.is_some() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
            s.poll(&mut Tx::default());
        }
        assert!(s.job.is_none(), "the operation never finished");
        settle(s);
    }

    fn go(s: &mut State, p: &Path) {
        s.go_to(p.to_path_buf());
        settle(s);
    }

    fn run(s: &mut State, cmd: Cmd) {
        assert_eq!(s.unavailable(cmd), None, "{cmd:?}");
        s.run(cmd, &mut Tx::default());
    }

    fn cursor(s: &State) -> String {
        s.cur().selected().map(|e| e.name.clone()).unwrap_or_default()
    }

    #[test]
    fn tab_lifecycle() {
        let (a, b) = (folder("life-a", &["a1", "a2"]), folder("life-b", &["b1"]));
        let mut s = state_in(&a);
        assert_eq!(s.unavailable(Cmd::CloseTab), Some("This is the only tab"));
        assert!(s.unavailable(Cmd::NextTab).is_some());
        run(&mut s, Cmd::NewTab);
        let p = &s.panes[0];
        assert_eq!((p.tabs.len(), p.tab), (2, 1));
        assert_eq!(s.cur().cwd, a, "a new tab opens on the same folder");
        assert_eq!(s.cur().entries.len(), 2, "with its listing, no read");
        go(&mut s, &b);
        assert_eq!(s.panes[0].tab_path(0), a.as_path());
        assert_eq!(s.panes[0].tab_path(1), b.as_path());
        // The hidden tab keeps no entries: only its memo.
        assert!(s.panes[0].tabs[0].is_some() && s.panes[0].tabs[1].is_none());
        run(&mut s, Cmd::NextTab); // wraps to the first
        settle(&mut s);
        assert_eq!((s.panes[0].tab, s.cur().cwd.clone()), (0, a.clone()));
        run(&mut s, Cmd::CloseTab);
        assert_eq!((s.panes[0].tabs.len(), s.panes[0].tab), (1, 0));
        settle(&mut s);
        assert_eq!(s.cur().cwd, b, "closing shows the neighbour");
        assert_eq!(s.unavailable(Cmd::CloseTab), Some("This is the only tab"));
        // ⌘W on the last tab is taken but does nothing.
        let ev = rune::KeyEvent { key: rune::Key::Command('w'), mods: rune::Mods { cmd: true, ..Default::default() } };
        if s.keymap.find(|c| c.matches_on(&ev, rune::KeyPlatform::current())) == Some(Cmd::CloseTab) {
            assert!(s.on_key(&ev, &mut Tx::default()));
            assert_eq!(s.panes[0].tabs.len(), 1);
        }
    }

    #[test]
    fn reopen_closed_tab_restores_its_state() {
        let (a, b) = (folder("reopen-a", &["x", "y", "z"]), folder("reopen-b", &["q"]));
        let mut s = state_in(&a);
        run(&mut s, Cmd::NewTab);
        go(&mut s, &b);
        run(&mut s, Cmd::PrevTab);
        settle(&mut s);
        s.cur_mut().select(2);
        assert_eq!(cursor(&s), "z");
        assert_eq!(s.unavailable(Cmd::ReopenTab), Some("No closed tabs to reopen"));
        run(&mut s, Cmd::CloseTab);
        settle(&mut s);
        assert_eq!(s.cur().cwd, b);
        run(&mut s, Cmd::ReopenTab);
        settle(&mut s);
        assert_eq!((s.panes[0].tabs.len(), s.panes[0].tab), (2, 1));
        assert_eq!(s.cur().cwd, a);
        assert_eq!(cursor(&s), "z", "the selection comes back by name");
        assert!(s.closed.is_empty());
    }

    #[test]
    fn each_tab_has_its_own_history() {
        let (a, b, c) = (folder("hist-a", &[]), folder("hist-b", &[]), folder("hist-c", &[]));
        let mut s = state_in(&a);
        go(&mut s, &b); // tab 1: a → b
        run(&mut s, Cmd::NewTab);
        assert!(s.cur().back.is_empty(), "a new tab starts its own history");
        go(&mut s, &c); // tab 2: b → c
        run(&mut s, Cmd::Back);
        settle(&mut s);
        assert_eq!(s.cur().cwd, b);
        assert_eq!(s.cur().fwd, vec![c.clone()]);
        run(&mut s, Cmd::PrevTab);
        settle(&mut s);
        assert_eq!(s.cur().cwd, b);
        assert_eq!(s.cur().back, vec![a.clone()], "tab 1's history is untouched");
        assert!(s.cur().fwd.is_empty());
        run(&mut s, Cmd::Back);
        settle(&mut s);
        assert_eq!(s.cur().cwd, a);
        run(&mut s, Cmd::NextTab);
        settle(&mut s);
        assert_eq!((s.cur().cwd.clone(), s.cur().fwd.clone()), (b, vec![c]));
    }

    /// Showing a hidden tab reads its folder again: a file made while it was hidden shows up,
    /// and the selection, filter and sort come back.
    #[test]
    fn showing_a_tab_reads_it_fresh_and_restores_the_selection() {
        let (a, b) = (folder("fresh-a", &["apple", "banana", "cherry"]), folder("fresh-b", &["other"]));
        let mut s = state_in(&a);
        s.show_wait = Duration::ZERO; // the read always lands later, through poll
        s.cur_mut().select(1);
        s.cur_mut().set_sort(crate::fs::SortKey::Size);
        let keep = cursor(&s);
        run(&mut s, Cmd::NewTab);
        go(&mut s, &b);
        std::fs::write(a.join("date"), "x").unwrap();
        run(&mut s, Cmd::PrevTab);
        // At once: the tab's path, an empty list, waiting for the read.
        assert_eq!(s.cur().cwd, a);
        assert!(s.cur().entries.is_empty() && s.is_loading());
        settle(&mut s);
        assert_eq!(s.cur().entries.len(), 4, "read fresh");
        assert_eq!(cursor(&s), keep);
        assert_eq!(s.cur().sort.key, crate::fs::SortKey::Size);
        assert!(s.cur().back.is_empty(), "showing a tab isn't a navigation");
    }

    #[test]
    fn a_local_folder_lands_in_the_same_frame() {
        let (a, b) = (folder("sameframe-a", &["one", "two"]), folder("sameframe-b", &[]));
        let mut s = state_in(&a);
        s.show_wait = Duration::from_secs(5); // a loaded CI machine: bounded, but generous
        s.cur_mut().select(1);
        run(&mut s, Cmd::NewTab);
        go(&mut s, &b);
        run(&mut s, Cmd::PrevTab);
        assert!(!s.is_loading(), "landed inside the switch");
        assert_eq!((s.cur().entries.len(), cursor(&s)), (2, "two".to_string()));
    }

    /// A file operation that ends while a tab's read is in flight reads it again, and the tab
    /// still gets its selection back.
    #[test]
    fn a_reload_while_showing_a_tab_keeps_its_selection() {
        let (a, b) = (folder("showreload-a", &["one", "two", "three"]), folder("showreload-b", &[]));
        let mut s = state_in(&a);
        s.show_wait = Duration::ZERO;
        s.cur_mut().select(2);
        let keep = cursor(&s);
        run(&mut s, Cmd::NewTab);
        go(&mut s, &b);
        run(&mut s, Cmd::PrevTab);
        crate::app::reload_folder(s.cur_mut(), Vec::new(), false);
        assert_eq!(s.cur().pending.as_ref().map(|p| p.nav), Some(crate::folder::Nav::Show));
        settle(&mut s);
        assert_eq!(cursor(&s), keep);
    }

    #[test]
    fn filter_and_multiple_selection_survive_hiding() {
        let a = folder("filter-a", &["red", "green", "blue", "grey"]);
        let mut s = state_in(&a);
        for c in ["g", "r"] {
            s.cur_mut().type_filter(c);
        }
        s.cur_mut().select_all();
        let picked = s.cur().marked.clone();
        assert_eq!(picked.len(), 2);
        run(&mut s, Cmd::NewTab);
        assert!(s.cur().filter.is_empty(), "the new tab starts unfiltered");
        run(&mut s, Cmd::PrevTab);
        settle(&mut s);
        assert_eq!(s.cur().filter, "gr");
        assert_eq!(s.cur().visible.len(), 2);
        assert_eq!(s.cur().marked, picked);
    }

    #[test]
    fn split_view_and_commander_commands() {
        let (a, b) = (folder("split-a", &["f1", "f2"]), folder("split-b", &[]));
        let mut s = state_in(&a);
        for cmd in [Cmd::CopyToOther, Cmd::MoveToOther, Cmd::OtherPane] {
            assert_eq!(s.unavailable(cmd), Some(crate::app::NOT_SPLIT), "{cmd:?}");
        }
        run(&mut s, Cmd::Split);
        assert_eq!((s.panes.len(), s.active), (2, 1), "the new pane is on the right, active");
        assert_eq!(s.cur().cwd, a);
        assert_eq!(s.cur().entries.len(), 2, "on the same folder, at once");
        assert_eq!(s.unavailable(Cmd::CopyToOther), Some("Both panes show the same folder"));
        go(&mut s, &b);
        run(&mut s, Cmd::OtherPane);
        assert_eq!((s.active, s.cur().cwd.clone()), (0, a.clone()));
        assert_eq!(s.panes[1].f.cwd, b, "each pane has its own folder");
        run(&mut s, Cmd::CopyToOther);
        let until = Instant::now() + Duration::from_secs(10);
        while s.job.is_some() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
            s.poll(&mut Tx::default());
        }
        settle(&mut s);
        assert!(b.join("f1").exists() && a.join("f1").exists());
        assert_eq!(s.panes[1].f.entries.len(), 1, "the other pane re-reads and shows it");
        assert_eq!(s.undo.len(), 1, "undoable");
        // Closing the split keeps the active pane; the other's tab can be reopened.
        run(&mut s, Cmd::Split);
        assert_eq!((s.panes.len(), s.active, s.cur().cwd.clone()), (1, 0, a.clone()));
        assert_eq!(s.closed.last().map(|m| m.path.clone()), Some(b.clone()));
        // ⌘W on a split pane's last tab closes that pane.
        run(&mut s, Cmd::Split);
        run(&mut s, Cmd::CloseTab);
        assert_eq!((s.panes.len(), s.active), (1, 0));
    }

    /// Paste (or Paste and Move) asked for in one pane lands in that pane's folder, even when
    /// the other pane is active by the time the clipboard answers.
    #[test]
    fn a_paste_lands_where_it_was_asked_for() {
        for (i, cmd) in [Cmd::Paste, Cmd::PasteMove].into_iter().enumerate() {
            let (a, b, src) =
                (folder(&format!("paste-a{i}"), &[]), folder(&format!("paste-b{i}"), &[]), folder(&format!("paste-src{i}"), &["f0"]));
            let mut s = state_in(&a);
            run(&mut s, Cmd::Split);
            go(&mut s, &b);
            run(&mut s, Cmd::OtherPane);
            assert_eq!(s.cur().cwd, a);
            run(&mut s, cmd);
            run(&mut s, Cmd::OtherPane); // B is active when the clipboard answers
            s.paste(&rune::ClipboardContent::new("").with_files(vec![src.join("f0")]));
            finish(&mut s);
            assert!(a.join("f0").exists(), "{cmd:?} lands in A");
            assert!(!b.join("f0").exists(), "{cmd:?} not in B");
            assert_eq!(src.join("f0").exists(), cmd == Cmd::Paste, "{cmd:?}: copies or moves as asked");
        }
    }

    /// An operation that ends while the other pane is on its way to another folder leaves that
    /// navigation alone, and re-reads only the panes showing a folder it changed.
    #[test]
    fn a_finished_operation_keeps_a_navigation_going() {
        let (a, b, c) = (folder("keepnav-a", &["f0"]), folder("keepnav-b", &[]), folder("keepnav-c", &["c0"]));
        let mut s = state_in(&a);
        run(&mut s, Cmd::Split);
        go(&mut s, &b);
        run(&mut s, Cmd::OtherPane);
        run(&mut s, Cmd::CopyToOther); // A's f0 into B
        run(&mut s, Cmd::OtherPane);
        s.go_to(c.clone()); // B's pane heads for C
        let until = Instant::now() + Duration::from_secs(10);
        while !s.poll_job() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(s.job.is_none());
        assert_eq!(s.pending_path(), Some(c.as_path()), "still on its way to C");
        assert!(s.panes[0].f.pending.is_none(), "A didn't change, so it isn't read again");
        settle(&mut s);
        assert_eq!(s.cur().cwd, c);
        assert!(b.join("f0").exists());
    }

    /// A navigation into the folder an operation changed is asked again, so what lands is read
    /// after the change.
    #[test]
    fn a_navigation_into_a_changed_folder_reads_it_again() {
        let (a, b) = (folder("renav-a", &["f0"]), folder("renav-b", &[]));
        let mut s = state_in(&a);
        run(&mut s, Cmd::Split);
        go(&mut s, &b);
        go(&mut s, &a);
        run(&mut s, Cmd::OtherPane);
        s.start(crate::ops::Op::Copy { sources: vec![a.join("f0")], dest: b.clone() });
        run(&mut s, Cmd::OtherPane);
        run(&mut s, Cmd::Back); // heads back to B
        let req = s.cur().pending.as_ref().map(|p| p.req);
        let until = Instant::now() + Duration::from_secs(10);
        while !s.poll_job() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
        }
        let p = s.cur().pending.as_ref().expect("still going");
        assert_eq!((p.path.clone(), p.nav), (b.clone(), crate::folder::Nav::Back));
        assert_ne!(Some(p.req), req, "asked again");
        settle(&mut s);
        assert_eq!(s.cur().cwd, b);
        assert_eq!(s.cur().entries.len(), 1, "the copy is there");
    }

    /// A tab hidden while its folder is being read stops that read: the rest of the folder
    /// isn't read off screen.
    #[test]
    fn hiding_a_tab_stops_its_read() {
        use crate::fs::read_hook::{GATES, Gate};
        use std::sync::mpsc::channel;
        let names: Vec<String> = (0..100).map(|i| format!("f{i}")).collect();
        let a = folder("stopread-a", &names.iter().map(String::as_str).collect::<Vec<_>>());
        let b = folder("stopread-b", &["b"]);
        let mut s = state_in(&a);
        s.show_wait = Duration::ZERO;
        run(&mut s, Cmd::NewTab);
        go(&mut s, &b);
        let ((started, wait_start), (go_on, go), (done, wait_done)) = (channel(), channel(), channel());
        GATES.lock().unwrap().push(Gate { path: a.clone(), started, go: Some(go), done, seen: 0 });
        run(&mut s, Cmd::PrevTab); // shows A: its read starts, and holds at the first entry
        wait_start.recv_timeout(Duration::from_secs(5)).expect("A's read started");
        run(&mut s, Cmd::NextTab); // hides A again
        go_on.send(()).unwrap();
        let seen = wait_done.recv_timeout(Duration::from_secs(5)).expect("A's read ended");
        assert_eq!(seen, 1, "stopped at once, not after reading all 100 entries");
        settle(&mut s);
        assert_eq!(s.cur().cwd, b);
    }

    /// Closing the pane a read is for stops it too.
    #[test]
    fn closing_a_pane_stops_its_read() {
        use crate::fs::read_hook::{GATES, Gate};
        use std::sync::mpsc::channel;
        let names: Vec<String> = (0..50).map(|i| format!("g{i}")).collect();
        let a = folder("stopread-pane-a", &["a"]);
        let c = folder("stopread-pane-c", &names.iter().map(String::as_str).collect::<Vec<_>>());
        let mut s = state_in(&a);
        run(&mut s, Cmd::Split);
        let ((started, wait_start), (go_on, go), (done, wait_done)) = (channel(), channel(), channel());
        GATES.lock().unwrap().push(Gate { path: c.clone(), started, go: Some(go), done, seen: 0 });
        s.go_to(c.clone());
        wait_start.recv_timeout(Duration::from_secs(5)).expect("C's read started");
        run(&mut s, Cmd::CloseTab); // the split pane's only tab: closes the pane
        assert_eq!(s.panes.len(), 1);
        go_on.send(()).unwrap();
        assert_eq!(wait_done.recv_timeout(Duration::from_secs(5)).expect("C's read ended"), 1);
    }

    fn rename_to(s: &mut State, name: &str) {
        s.start_rename();
        s.rename.as_mut().expect("renaming").edit.text = name.into();
        s.commit_rename(&mut Tx::default());
    }

    /// Both panes on one folder: a rename in one re-reads the other too, so no row there names
    /// a file that's gone (or, worse, now another file), and its selection follows the item.
    #[test]
    fn a_rename_refreshes_the_other_pane_on_the_same_folder() {
        let a = folder("ren-other", &[]);
        std::fs::write(a.join("f0"), "first").unwrap();
        std::fs::write(a.join("f1"), "second-file-different-length").unwrap();
        let mut s = state_in(&a);
        run(&mut s, Cmd::Split);
        assert_eq!(cursor(&s), "f0");
        rename_to(&mut s, "renamed-first");
        // At once, before any read lands: the other pane shows the new name, still selected.
        assert_eq!(s.panes[0].f.selected().map(|e| e.name.as_str()), Some("renamed-first"));
        settle(&mut s);
        let i = s.cur().visible.iter().position(|&i| s.cur().entries[i as usize].name == "f1").unwrap();
        s.cur_mut().select(i);
        rename_to(&mut s, "f0");
        settle(&mut s);
        run(&mut s, Cmd::OtherPane);
        let mut rows: Vec<(String, u64)> = s.cur().entries.iter().map(|e| (e.name.clone(), e.size)).collect();
        rows.sort();
        assert_eq!(rows, [("f0".to_string(), 28), ("renamed-first".to_string(), 5)], "what's on disk");
        assert_eq!(cursor(&s), "renamed-first");
        assert_eq!(std::fs::read_to_string(&s.target_paths()[0]).unwrap(), "first", "its actions act on the file it shows");
    }

    /// One pane on a folder, the other on a symlink to it: one folder, so a rename in either
    /// re-reads both, and no row in the pane on the real path names a file that's gone (or now
    /// another file). The same for a new folder, which goes through `refresh` like every file
    /// operation.
    #[cfg(unix)]
    #[test]
    fn a_pane_on_a_symlink_to_the_folder_is_the_same_folder() {
        let a = folder("ren-alias", &[]);
        std::fs::write(a.join("f0"), "first").unwrap();
        std::fs::write(a.join("f1"), "second-file").unwrap();
        let alias = a.with_extension("link");
        let _ = std::fs::remove_file(&alias);
        std::os::unix::fs::symlink(&a, &alias).unwrap();
        let mut s = state_in(&a);
        run(&mut s, Cmd::Split);
        go(&mut s, &alias);
        assert_eq!(s.unavailable(Cmd::CopyToOther), Some("Both panes show the same folder"));
        rename_to(&mut s, "renamed-first");
        assert_eq!(s.panes[0].f.selected().map(|e| e.name.as_str()), Some("renamed-first"), "at once, by identity");
        settle(&mut s);
        let i = s.cur().visible.iter().position(|&i| s.cur().entries[i as usize].name == "f1").unwrap();
        s.cur_mut().select(i);
        rename_to(&mut s, "f0");
        settle(&mut s);
        let real = &s.panes[0].f;
        let mut rows: Vec<(String, u64)> = real.entries.iter().map(|e| (e.name.clone(), e.size)).collect();
        rows.sort();
        assert_eq!(rows, [("f0".to_string(), 11), ("renamed-first".to_string(), 5)], "the real path's pane shows what's on disk");
        let e = real.selected().unwrap();
        assert_eq!(e.name, "renamed-first");
        assert_eq!(std::fs::read_to_string(&real.target_paths()[0]).unwrap(), "first", "and acts on the file it shows");
        // A new folder made through the link shows up in the pane on the real path.
        run(&mut s, Cmd::NewFolder);
        settle(&mut s);
        s.rename = None;
        assert!(s.panes[0].f.entries.iter().any(|e| e.name == "untitled folder"));
    }

    /// A new folder shows up in the other pane on the same folder too.
    #[test]
    fn a_new_folder_refreshes_the_other_pane_on_the_same_folder() {
        let a = folder("nf-other", &["x"]);
        let mut s = state_in(&a);
        run(&mut s, Cmd::Split);
        run(&mut s, Cmd::NewFolder);
        settle(&mut s);
        assert!(s.rename.is_some(), "renaming the new folder");
        assert_eq!(s.panes[0].f.entries.len(), 2);
    }

    /// A handler from a frame drawn before a pane closed names a pane that's gone: nothing.
    #[test]
    fn stale_pane_handlers_do_nothing() {
        let a = folder("stale", &["f"]);
        let mut s = state_in(&a);
        run(&mut s, Cmd::Split);
        let gone = s.panes[1 - s.active].id();
        run(&mut s, Cmd::Split); // closes the inactive pane
        assert!(s.pane_ix(gone).is_none());
        assert!(!s.activate_id(gone, &mut Tx::default()));
        s.drop_on_pane(gone, 0, 0, s.panes[0].id(), &mut Tx::default());
        assert!(s.job.is_none());
    }
}
