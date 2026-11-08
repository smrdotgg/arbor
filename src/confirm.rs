//! The one kind of dialog Arbor shows (NORTHSTAR Safety #2): before anything that can't be
//! undone, it asks, and says plainly that it can't be undone.
//!
//! A command that can't be undone is marked in the command table ([`Cmd::permanent`]). Running
//! it from a key, the palette or the context menu opens this dialog instead (`State::run`), and
//! only the dialog's destructive button runs it. Future permanent actions (deleting
//! permanently, emptying the Trash, a volume with no Recycle Bin) plug in here: one line in
//! `permanent`, and their words in [`Ask::for_cmd`].
//!
//! Cancel has the focus when it opens: Enter cancels, and only Tab (or ← →) and then Enter, or
//! a click on the button, confirms. An Enter meant for something else (the palette's Enter that
//! ran the command a moment ago, pressed twice) or pressed out of habit then can't destroy
//! anything. Esc, or a click outside, cancels. Nothing animates.

use rune::{Key, KeyEvent, Tx};

use crate::app::State;
use crate::commands::Cmd;

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
    /// One or two sentences, ending "This can't be undone."
    pub body: String,
    /// The destructive button's label.
    pub action: &'static str,
    pub focus: Button,
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
            // Not permanent: never asked (see `Cmd::permanent`).
            _ => (format!("{}?", cmd.title().trim_end_matches('…')), "This can't be undone.".to_string(), "Continue"),
        };
        Ask { cmd, title, body, action, focus: Button::Cancel }
    }
}

impl State {
    /// Opens the dialog for permanent command `cmd` (instead of running it).
    pub(crate) fn ask(&mut self, cmd: Cmd, tx: &mut Tx) {
        self.close_overlays(tx);
        if self.rename.take().is_some() {
            tx.blur();
        }
        // A search field lets go of the keys: they're the dialog's now.
        let ix = self.active;
        self.focus_results(ix, tx);
        self.asking = Some(Ask::for_cmd(self, cmd));
    }

    /// Closes the dialog; with `yes`, runs what it asked about (if it can still run).
    pub fn answer(&mut self, yes: bool, tx: &mut Tx) {
        tx.instant();
        let Some(a) = self.asking.take() else { return };
        if yes {
            match self.unavailable(a.cmd) {
                Some(why) => self.note = Some(why.into()),
                None => self.run_permanent(a.cmd, tx),
            }
        }
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
        assert_eq!(permanent, [Cmd::ClearJump]);
        assert!(Cmd::ClearJump.title().ends_with('…'), "a command that asks says so in its title");
    }
}
