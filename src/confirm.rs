//! The one kind of dialog Arbor shows (NORTHSTAR Safety #2): before anything that can't be
//! undone, it asks, and says plainly that it can't be undone. It also asks before the two
//! commands that change how the system opens folders (`handler.rs`), saying what changes and
//! which command undoes it.
//!
//! A command that can't be undone is marked in the command table ([`Cmd::permanent`]). Running
//! it from a key, the palette or the context menu opens this dialog instead (`State::run`), and
//! only the dialog's destructive button runs it. A new permanent action plugs in here: one line
//! in `permanent`, and its words in [`Ask::for_cmd`].
//!
//! Deleting permanently asks with the items it names ([`Ask::delete`]), whether from "Delete
//! Permanently…" or after a trash left items whose disk has no trash; the button deletes
//! exactly those, each only while it's still the item named.
//!
//! Cancel has the focus when it opens: Enter cancels, and only Tab (or ← →) and then Enter, or
//! a click on the button, confirms. An Enter meant for something else (the palette's Enter that
//! ran the command a moment ago, pressed twice) or pressed out of habit then can't destroy
//! anything. Esc, or a click outside, cancels. Nothing animates.

use rune::{Key, KeyEvent, Tx};

use crate::app::State;
use crate::commands::Cmd;
use crate::ops::{Doomed, Op};
use crate::trash::BIN;

/// The dialog's two buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Cancel,
    Action,
}

/// An open confirmation: what it asks about, in words, and which button has the focus.
pub struct Ask {
    pub cmd: Cmd,
    pub title: String,
    /// One or two sentences: for what can't be undone, ending "This can't be undone."; for a
    /// system change, what changes and how to undo it.
    pub body: String,
    /// The action button's label.
    pub action: &'static str,
    pub focus: Button,
    /// For a permanent delete, what it deletes (and nothing else).
    pub items: Vec<Doomed>,
}

impl Ask {
    /// What `cmd` asks, with what's here filled in.
    fn for_cmd(s: &State, cmd: Cmd) -> Ask {
        let (title, body, action) = match cmd {
            Cmd::ClearJump => {
                let what = match s.history.agg.as_ref().map(|a| a.len()) {
                    Some(1) => "the folder you've been to".to_string(),
                    Some(n) => format!("the {} folders you've been to", crate::search::count(n as u64)),
                    None => "every folder you've been to".to_string(),
                };
                ("Clear Jump History?".to_string(), format!("Jump to Folder forgets {what}. This can't be undone."), "Clear History")
            }
            Cmd::Delete => return Ask::delete(s.cur().doomed(), None),
            Cmd::EmptyTrash => {
                let (title, body, action) = crate::trash::contents::ask_words(s.trash_count.as_ref().map(|c| &c.got));
                (title, body, action)
            }
            Cmd::MakeDefault => ("Make Arbor the Default for Folders?".into(), MAKE_DEFAULT.into(), "Make Default"),
            Cmd::RestoreDefault => ("Restore the System Default for Folders?".into(), RESTORE_DEFAULT.into(), "Restore"),
            // Not permanent: never asked (see `Cmd::permanent`).
            _ => (format!("{}?", cmd.title().trim_end_matches('…')), "This can't be undone.".to_string(), "Continue"),
        };
        Ask { cmd, title, body, action, focus: Button::Cancel, items: Vec::new() }
    }

    /// Deleting `items` permanently: from "Delete Permanently…" (`no_trash` is `None`), or for
    /// items a trash left where they were because their disk has none (`no_trash` says why).
    /// Names the first few and the count, says folders go with everything in them, and that it
    /// can't be undone.
    pub(crate) fn delete(items: Vec<Doomed>, no_trash: Option<&str>) -> Ask {
        let n = items.len();
        let names = names(&items);
        let inside = match (items.iter().filter(|d| d.dir).count(), n) {
            (0, _) => "",
            (_, 1) => ", with everything in it",
            _ => ", folders with everything in them",
        };
        let (title, body) = match no_trash {
            None if n == 1 => {
                (format!("Delete {names} Permanently?"), format!("{names} will be deleted permanently{inside}. This can't be undone."))
            }
            None => (
                format!("Delete {} Items Permanently?", crate::search::count(n as u64)),
                format!("{names} will be deleted permanently{inside}. This can't be undone."),
            ),
            Some(why) => {
                let them = if n == 1 { "it" } else { "them" };
                (
                    format!("Can't Move to the {BIN}"),
                    format!("{names} can't go to the {BIN}: {why}. Delete {them} permanently{inside}? This can't be undone."),
                )
            }
        };
        Ask { cmd: Cmd::Delete, title, body, action: "Delete Permanently", focus: Button::Cancel, items }
    }
}

/// What "Make Arbor the Default for Folders" changes on this platform, and how to undo it.
const MAKE_DEFAULT: &str = if cfg!(target_os = "macos") {
    "Folders that other apps open (Open With, `open`, a download's folder) will open in Arbor. Finder still opens the desktop, the Dock's folders and \"Show in Finder\" from other apps: macOS doesn't let another app take those. To undo it, run \"Restore the System Default for Folders\"."
} else if cfg!(windows) {
    "Opening a folder or a drive (in Explorer, on the desktop, from other apps) and Win+E will open Arbor instead of Explorer. Only your account's settings change (a few keys under HKEY_CURRENT_USER), and what they held is saved. To undo it, run \"Restore the System Default for Folders\"."
} else {
    "Folders that other apps open, and \"Show in folder\" in browsers and other apps, will open in Arbor. It sets your folder default in ~/.config/mimeapps.list and adds a D-Bus file, saving what was there. To undo it, run \"Restore the System Default for Folders\"."
};

/// What "Restore the System Default for Folders" does.
const RESTORE_DEFAULT: &str = if cfg!(target_os = "macos") {
    "Folders will open in the app that opened them before Arbor (Finder unless you'd chosen another). You can make Arbor the default again at any time."
} else if cfg!(windows) {
    "Folders, drives and Win+E will open in Explorer again: your account's settings go back to what they were before Arbor changed them. You can make Arbor the default again at any time."
} else {
    "Folders will open in the file manager you had before Arbor, and \"Show in folder\" will stop opening Arbor. You can make Arbor the default again at any time."
};

/// The first few items' names and how many more: "“a”", "“a” and “b”", "“a”, “b”, “c” and 4 more".
fn names(items: &[Doomed]) -> String {
    const SHOWN: usize = 3;
    let q: Vec<String> = items
        .iter()
        .take(SHOWN)
        .map(|d| format!("“{}”", d.path.file_name().map_or_else(|| d.path.display().to_string(), |n| n.to_string_lossy().into_owned())))
        .collect();
    let more = items.len().saturating_sub(SHOWN);
    match (q.len(), more) {
        (0, _) => "Nothing".into(),
        (1, 0) => q[0].clone(),
        (k, 0) => format!("{} and {}", q[..k - 1].join(", "), q[k - 1]),
        (_, m) => format!("{} and {} more", q.join(", "), crate::search::count(m as u64)),
    }
}

impl State {
    /// Opens the dialog for permanent command `cmd` (instead of running it).
    pub(crate) fn ask(&mut self, cmd: Cmd, tx: &mut Tx) {
        let a = Ask::for_cmd(self, cmd);
        self.ask_with(a, tx);
        // Empty Trash: what's in it is counted on a thread of its own; the dialog is already
        // up and says the count when it lands (`poll_trash_count`).
        if cmd == Cmd::EmptyTrash {
            self.trash_count = Some(crate::trash::contents::Counting::start(self.waker.clone()));
        }
    }

    /// The Trash's count, while Empty Trash's dialog is open: its words follow what's found.
    /// Once the dialog has closed, the count stops. True when anything changed.
    pub(crate) fn poll_trash_count(&mut self) -> bool {
        let Some(c) = self.trash_count.as_mut() else { return false };
        let Some(a) = self.asking.as_mut().filter(|a| a.cmd == Cmd::EmptyTrash) else {
            self.trash_count = None;
            return false;
        };
        if !c.poll() {
            return false;
        }
        let (title, body, action) = crate::trash::contents::ask_words(Some(&c.got));
        (a.title, a.body, a.action) = (title, body, action);
        true
    }

    /// Opens the dialog asking `a`.
    pub(crate) fn ask_with(&mut self, a: Ask, tx: &mut Tx) {
        self.close_overlays(tx);
        if self.rename.take().is_some() {
            tx.blur();
        }
        // A search field lets go of the keys: they're the dialog's now.
        let ix = self.active;
        self.focus_results(ix, tx);
        self.asking = Some(a);
    }

    /// Closes the dialog; with `yes`, runs what it asked about (if it can still run).
    pub fn answer(&mut self, yes: bool, tx: &mut Tx) {
        tx.instant();
        let Some(a) = self.asking.take() else { return };
        // A delete deletes what the dialog named, whatever is selected now.
        if yes && a.cmd == Cmd::Delete {
            if self.job.is_some() {
                self.note = Some(crate::app::BUSY.into());
            } else if !a.items.is_empty() {
                self.start(Op::Delete { items: a.items });
            }
            return;
        }
        if yes {
            match self.unavailable(a.cmd) {
                Some(why) => self.note = Some(why.into()),
                None => self.run_permanent(a.cmd, tx),
            }
        }
        // Its count stops (Empty Trash took what it found, if it ran).
        self.trash_count = None;
    }

    /// The dialog's keys (it's modal: it takes every key while open).
    pub fn ask_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        tx.instant();
        let Some(a) = self.asking.as_mut() else { return false };
        let m = ev.mods;
        let plain = !m.cmd && !m.ctrl && !m.alt;
        match &ev.key {
            Key::Escape => self.answer(false, tx),
            Key::Tab | Key::Left | Key::Right if !m.cmd && !m.ctrl => {
                a.focus = if a.focus == Button::Cancel { Button::Action } else { Button::Cancel };
            }
            Key::Enter if plain => {
                let yes = a.focus == Button::Action;
                self.answer(yes, tx);
            }
            Key::Text(t) if t == " " && plain => {
                let yes = a.focus == Button::Action;
                self.answer(yes, tx);
            }
            _ => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Nav;
    use crate::app::tests::{settle_all, state_in};

    fn key(k: Key) -> KeyEvent {
        KeyEvent { key: k, mods: rune::Mods::default() }
    }

    fn history_of_one(name: &str) -> State {
        let d = std::env::temp_dir().join(format!("arbor-confirm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("a")).unwrap();
        let mut s = state_in(&d);
        s.go(d.join("a"), Nav::New, None);
        settle_all(&mut s);
        let mut tx = Tx::default();
        s.run(Cmd::Jump, &mut tx);
        s.close_jump(&mut tx);
        assert_eq!(s.history.agg.as_ref().unwrap().len(), 1);
        s
    }

    /// From the palette, as most people run it: it asks, Enter (on Cancel) keeps the history,
    /// and only the destructive button clears it. Running it twice from the palette, which used
    /// to be the way to confirm, never clears.
    #[test]
    fn clear_jump_history_asks_and_clears_only_on_the_destructive_button() {
        let mut s = history_of_one("palette");
        let mut tx = Tx::default();
        for _ in 0..2 {
            s.run(Cmd::Palette, &mut tx);
            s.palette_pick(Cmd::ClearJump, &mut tx);
            assert!(s.palette.is_none());
            let a = s.asking.as_ref().expect("it asks");
            assert_eq!(a.title, "Clear Jump History?");
            assert!(a.body.contains("the folder you've been to") && a.body.ends_with("This can't be undone."), "{}", a.body);
            assert_eq!(a.focus, Button::Cancel, "Cancel has the focus");
            assert_eq!(s.history.agg.as_ref().unwrap().len(), 1, "nothing cleared by asking");
        }
        // Enter on Cancel cancels; so does Esc.
        assert!(s.on_key(&key(Key::Enter), &mut tx));
        assert!(s.asking.is_none());
        assert_eq!(s.history.agg.as_ref().unwrap().len(), 1);
        s.run(Cmd::ClearJump, &mut tx);
        assert!(s.on_key(&key(Key::Escape), &mut tx));
        assert!(s.asking.is_none());
        assert_eq!(s.history.agg.as_ref().unwrap().len(), 1);
        // Other keys don't leak to the window while it's open.
        s.run(Cmd::ClearJump, &mut tx);
        let sel = s.cur().sel;
        assert!(s.on_key(&key(Key::Down), &mut tx));
        assert!(s.on_key(&key(Key::Text("x".into())), &mut tx));
        assert_eq!((s.cur().sel, s.cur().filter.as_str()), (sel, ""));
        // Tab to the destructive button, Enter: cleared.
        assert!(s.on_key(&key(Key::Tab), &mut tx));
        assert_eq!(s.asking.as_ref().unwrap().focus, Button::Action);
        assert!(s.on_key(&key(Key::Enter), &mut tx));
        assert!(s.asking.is_none());
        assert!(s.history.agg.as_ref().unwrap().is_empty());
    }

    #[test]
    fn a_click_on_the_button_confirms_and_permanent_commands_are_marked() {
        let mut s = history_of_one("click");
        let mut tx = Tx::default();
        s.run(Cmd::ClearJump, &mut tx);
        s.answer(true, &mut tx);
        assert!(s.history.agg.as_ref().unwrap().is_empty());
        let permanent: Vec<Cmd> = Cmd::ALL.iter().copied().filter(|c| c.permanent()).collect();
        assert_eq!(permanent, [Cmd::ClearJump, Cmd::Delete, Cmd::EmptyTrash]);
        let asking: Vec<Cmd> = Cmd::ALL.iter().copied().filter(|c| c.asks()).collect();
        assert_eq!(asking, [Cmd::ClearJump, Cmd::MakeDefault, Cmd::RestoreDefault, Cmd::Delete, Cmd::EmptyTrash]);
        assert!(asking.iter().all(|c| c.title().ends_with('…')), "a command that asks says so in its title");
        assert!(Cmd::ClearJump.title().ends_with('…'), "a command that asks says so in its title");
    }

    /// Changing the folder default is never silent: it asks, says what changes and how to undo
    /// it (not "can't be undone"), and Enter on Cancel changes nothing.
    #[test]
    fn the_folder_default_commands_ask_and_say_how_to_undo() {
        let (d, mut s) = folder_of("default", &["a"]);
        let mut tx = Tx::default();
        s.run(Cmd::MakeDefault, &mut tx);
        let a = s.asking.as_ref().expect("it asks");
        assert_eq!((a.title.as_str(), a.action, a.focus), ("Make Arbor the Default for Folders?", "Make Default", Button::Cancel));
        assert!(
            a.body.contains("To undo it, run \"Restore the System Default for Folders\"") && !a.body.contains("can't be undone"),
            "{}",
            a.body
        );
        assert!(s.on_key(&key(Key::Enter), &mut tx));
        assert!(s.asking.is_none() && s.note.is_none(), "Cancel changed nothing");
        s.run(Cmd::RestoreDefault, &mut tx);
        assert_eq!(s.asking.as_ref().unwrap().title, "Restore the System Default for Folders?");
        // The button runs it on a worker; tests never touch the real system.
        s.answer(true, &mut tx);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while s.note.as_deref() == Some("Restoring the system default for folders…") && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(2));
            s.poll(&mut tx);
        }
        assert_eq!(s.note.as_deref(), Some("The default for folders isn't changed in tests or headless runs"));
        let _ = std::fs::remove_dir_all(&d);
    }

    fn settle_job(s: &mut State) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while (s.job.is_some() || s.ask_next.is_some()) && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(2));
            s.poll(&mut Tx::default());
        }
        assert!(s.job.is_none(), "the operation never finished");
        settle_all(s);
    }

    fn folder_of(name: &str, files: &[&str]) -> (std::path::PathBuf, State) {
        let d = std::env::temp_dir().join(format!("arbor-confirm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for f in files {
            match f.strip_suffix('/') {
                Some(dir) => std::fs::create_dir_all(d.join(dir).join("inside")).unwrap(),
                None => std::fs::write(d.join(f), f).unwrap(),
            }
        }
        let mut s = state_in(&d);
        settle_all(&mut s);
        (d, s)
    }

    /// Delete Permanently always asks, naming the items and saying it can't be undone; Enter
    /// (on Cancel) keeps them, and only the destructive button deletes, with no undo entry.
    #[test]
    fn delete_permanently_asks_and_deletes_only_on_the_button() {
        let (d, mut s) = folder_of("delete", &["a.txt", "b.txt", "sub/"]);
        let mut tx = Tx::default();
        s.run(Cmd::SelectAll, &mut tx);
        let undos = s.undo.len();
        s.run(Cmd::Delete, &mut tx);
        let a = s.asking.as_ref().expect("it asks");
        assert_eq!(a.title, "Delete 3 Items Permanently?");
        for n in ["“a.txt”", "“b.txt”", "“sub”"] {
            assert!(a.body.contains(n), "{}", a.body);
        }
        assert!(a.body.contains("folders with everything in them") && a.body.ends_with("This can't be undone."), "{}", a.body);
        assert_eq!((a.focus, a.action), (Button::Cancel, "Delete Permanently"));
        assert!(s.on_key(&key(Key::Enter), &mut tx));
        assert!(s.asking.is_none() && d.join("a.txt").exists() && d.join("sub/inside").exists());
        s.run(Cmd::Delete, &mut tx);
        assert!(s.on_key(&key(Key::Tab), &mut tx));
        assert!(s.on_key(&key(Key::Enter), &mut tx));
        settle_job(&mut s);
        assert!(!d.join("a.txt").exists() && !d.join("b.txt").exists() && !d.join("sub").exists());
        assert_eq!(s.note.as_deref(), Some("Deleted 3 items permanently"));
        assert_eq!(s.undo.len(), undos, "nothing to undo");
        assert!(s.cur().marked.is_empty(), "what's gone isn't selected: {:?}", s.cur().marked);
        // One item: named in the title.
        std::fs::write(d.join("c.txt"), "c").unwrap();
        s.reload();
        settle_all(&mut s);
        s.run(Cmd::SelectAll, &mut tx);
        s.run(Cmd::Delete, &mut tx);
        let a = s.asking.as_ref().unwrap();
        assert_eq!(a.title, "Delete “c.txt” Permanently?");
        assert_eq!(a.body, "“c.txt” will be deleted permanently. This can't be undone.");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A trash whose items' disk has no trash: the others go to the trash, and the rest are
    /// never deleted without the dialog, which names them and says why; Esc keeps them.
    #[test]
    fn a_trash_the_disk_cant_take_asks_before_deleting() {
        let (d, mut s) = folder_of("no-trash", &["a", "notrash-b", "notrash-c"]);
        let mut tx = Tx::default();
        s.run(Cmd::SelectAll, &mut tx);
        s.run(Cmd::Trash, &mut tx);
        settle_job(&mut s);
        assert!(d.join(".trash/a").exists(), "the one that can go, went");
        assert_eq!(s.note.as_deref(), Some(&*format!("Moved 1 item to the {BIN}")));
        let a = s.asking.as_ref().expect("it asks about the rest");
        assert_eq!(a.title, format!("Can't Move to the {BIN}"));
        assert_eq!(
            a.body,
            format!(
                "“notrash-b” and “notrash-c” can't go to the {BIN}: the test disk has no trash. Delete them permanently? This can't be undone."
            )
        );
        assert_eq!(a.focus, Button::Cancel);
        assert!(s.on_key(&key(Key::Escape), &mut tx));
        assert!(d.join("notrash-b").exists() && d.join("notrash-c").exists(), "Esc deletes nothing");
        // Again, and this time the button.
        s.run(Cmd::SelectAll, &mut tx);
        s.run(Cmd::Trash, &mut tx);
        settle_job(&mut s);
        assert_eq!(s.note.as_deref(), Some(&*format!("Nothing was moved to the {BIN}")));
        s.answer(true, &mut tx);
        settle_job(&mut s);
        assert!(!d.join("notrash-b").exists() && !d.join("notrash-c").exists());
        assert_eq!(s.note.as_deref(), Some("Deleted 2 items permanently"));
        // Undo puts back only what went to the trash.
        s.run(Cmd::Undo, &mut tx);
        settle_job(&mut s);
        assert!(d.join("a").exists() && !d.join("notrash-b").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Empty Trash asks at once (the dialog never waits for the count), says the count and
    /// size once a thread has counted, keeps everything on Enter (Cancel), and only the
    /// destructive button empties, with a truthful result and nothing to undo. Then an empty
    /// Trash says so. Fixture trashes only (`contents::FIXTURE`): never the user's own.
    #[test]
    fn empty_trash_asks_counts_and_empties_only_on_the_button() {
        use crate::trash::contents::{Bin, FIXTURE};
        let (d, mut s) = folder_of("empty-trash", &[]);
        let root = d.join(".Trash-1000");
        std::fs::create_dir_all(root.join("files/sub")).unwrap();
        std::fs::create_dir_all(root.join("info")).unwrap();
        for n in ["a", "b"] {
            std::fs::write(root.join("files").join(n), vec![b'x'; 512]).unwrap();
            std::fs::write(root.join("info").join(format!("{n}.trashinfo")), "[Trash Info]\nPath=/x\n").unwrap();
        }
        std::fs::write(root.join("files/sub/c"), vec![b'x'; 1024]).unwrap();
        *FIXTURE.lock().unwrap() = vec![Bin { items: root.join("files"), info: Some(root.join("info")) }];
        let mut tx = Tx::default();
        let undos = s.undo.len();
        let counted = |s: &mut State| {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !s.asking.as_ref().is_some_and(|a| a.body.contains("KB")) && std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(2));
                s.poll(&mut Tx::default());
            }
        };
        s.run(Cmd::EmptyTrash, &mut tx);
        let a = s.asking.as_ref().expect("it asks at once");
        assert_eq!((a.title.as_str(), a.focus), (&*format!("Empty the {BIN}?"), Button::Cancel));
        assert!(a.body.ends_with("This can't be undone."), "{}", a.body);
        counted(&mut s);
        assert_eq!(
            s.asking.as_ref().unwrap().body,
            format!("The 3 items in the {BIN} (2.0 KB) will be deleted permanently. This can't be undone.")
        );
        assert!(s.on_key(&key(Key::Enter), &mut tx), "Enter on Cancel");
        assert!(s.asking.is_none() && s.trash_count.is_none());
        assert!(root.join("files/a").exists() && root.join("files/sub/c").exists(), "Cancel deletes nothing");
        s.run(Cmd::EmptyTrash, &mut tx);
        counted(&mut s);
        assert!(s.on_key(&key(Key::Tab), &mut tx));
        assert!(s.on_key(&key(Key::Enter), &mut tx));
        settle_job(&mut s);
        assert_eq!(s.note.as_deref(), Some(&*format!("Emptied the {BIN}: deleted 3 items permanently")));
        assert_eq!(std::fs::read_dir(root.join("files")).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(root.join("info")).unwrap().count(), 0, "their records went too");
        assert_eq!(s.undo.len(), undos, "nothing to undo");
        // Now empty: the dialog says so, and so does the button.
        s.run(Cmd::EmptyTrash, &mut tx);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !s.asking.as_ref().is_some_and(|a| a.body.contains("already empty")) && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(2));
            s.poll(&mut Tx::default());
        }
        assert_eq!(s.asking.as_ref().unwrap().body, format!("The {BIN} is already empty."));
        s.answer(true, &mut tx);
        settle_job(&mut s);
        assert_eq!(s.note.as_deref(), Some(&*format!("The {BIN} is already empty")));
        FIXTURE.lock().unwrap().clear();
        let _ = std::fs::remove_dir_all(&d);
    }
}
