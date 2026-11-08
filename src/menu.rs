//! The context menu where Rune has no native one (Linux, see RUNE-GAPS.md): a small card in the
//! window at the pointer, with the same items as the native menus on macOS and Windows
//! (`State::menu_items`), drawn by `menu_view` in `view.rs`.
//!
//! As a native menu does: nothing is highlighted when it opens; ↑ ↓ move (skipping separators
//! and what can't run), Home and End go to the ends, a letter goes to the next item starting
//! with it, Enter or Space runs the highlighted item, and Esc or a click outside closes it. It
//! takes every key while open. Nothing animates, and it costs nothing while closed.

use rune::{Key, KeyEvent, PointerPress, Tx};

use crate::app::State;
use crate::commands::Cmd;

/// One row: a command (its title for what's selected, whether it can run, its keys as this
/// platform draws them), or a separator.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Cmd { cmd: Cmd, title: String, on: bool, keys: String },
    Separator,
}

/// An open menu: where it was asked for (window points), its rows, the highlighted one.
#[derive(Debug, Clone, PartialEq)]
pub struct Menu {
    pub x: f32,
    pub y: f32,
    pub items: Vec<Item>,
    pub sel: Option<usize>,
}

impl Menu {
    /// The rows that can run, in order.
    fn live(&self) -> impl Iterator<Item = usize> + '_ {
        self.items.iter().enumerate().filter(|(_, it)| matches!(it, Item::Cmd { on: true, .. })).map(|(i, _)| i)
    }

    /// Highlights the next (`down`) or previous row that can run, wrapping; from nothing, the
    /// first or last.
    fn step(&mut self, down: bool) {
        let live: Vec<usize> = self.live().collect();
        let (Some(&first), Some(&last)) = (live.first(), live.last()) else { return };
        self.sel = Some(match (self.sel, down) {
            (None, true) => first,
            (None, false) => last,
            (Some(s), true) => live.iter().copied().find(|&i| i > s).unwrap_or(first),
            (Some(s), false) => live.iter().rev().copied().find(|&i| i < s).unwrap_or(last),
        });
    }

    /// Highlights the next row that can run whose title starts with `c` (case aside), after the
    /// highlighted one, wrapping.
    fn jump(&mut self, c: char) {
        let c = c.to_lowercase().next().unwrap_or(c);
        let starts = |i: usize| matches!(&self.items[i], Item::Cmd { title, .. } if title.to_lowercase().starts_with(c));
        let live: Vec<usize> = self.live().filter(|&i| starts(i)).collect();
        let after = self.sel.and_then(|s| live.iter().copied().find(|&i| i > s));
        if let Some(i) = after.or(live.first().copied()) {
            self.sel = Some(i);
        }
    }
}

impl State {
    /// Opens the in-window menu at `press` with `items` (from `State::menu_items`).
    pub(crate) fn open_menu(&mut self, items: Vec<Option<(Cmd, String, bool)>>, press: PointerPress, tx: &mut Tx) {
        self.close_overlays(tx);
        let items = items
            .into_iter()
            .map(|it| match it {
                Some((cmd, title, on)) => Item::Cmd { cmd, title, on, keys: self.keymap.label(cmd) },
                None => Item::Separator,
            })
            .collect();
        self.menu = Some(Menu { x: press.x, y: press.y, items, sel: None });
    }

    pub fn close_menu(&mut self) {
        self.menu = None;
    }

    /// Runs row `i`'s command (a click, or Enter on the highlight) if it can run now.
    pub fn menu_pick(&mut self, i: usize, tx: &mut Tx) {
        tx.instant();
        let Some(m) = self.menu.take() else { return };
        if let Some(Item::Cmd { cmd, on: true, .. }) = m.items.get(i)
            && self.unavailable(*cmd).is_none()
        {
            self.run(*cmd, tx);
        }
    }

    /// The menu's keys (it takes every key while open).
    pub fn menu_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        tx.instant();
        let Some(m) = self.menu.as_mut() else { return false };
        let plain = !ev.mods.cmd && !ev.mods.ctrl && !ev.mods.alt;
        match &ev.key {
            Key::Escape => self.menu = None,
            Key::Down => m.step(true),
            Key::Up => m.step(false),
            Key::Home => {
                m.sel = None;
                m.step(true);
            }
            Key::End => {
                m.sel = None;
                m.step(false);
            }
            Key::Enter if plain => {
                if let Some(i) = m.sel {
                    self.menu_pick(i, tx);
                }
            }
            Key::Text(t) if t == " " && plain => {
                if let Some(i) = m.sel {
                    self.menu_pick(i, tx);
                }
            }
            Key::Text(t) if plain => {
                if let Some(c) = t.chars().next() {
                    m.jump(c);
                }
            }
            _ => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{settle_all, state_in};

    fn key(k: Key) -> KeyEvent {
        KeyEvent { key: k, mods: rune::Mods::default() }
    }

    fn titles(m: &Menu) -> Vec<String> {
        m.items
            .iter()
            .map(|it| match it {
                Item::Cmd { title, on, .. } => format!("{title}{}", if *on { "" } else { " (off)" }),
                Item::Separator => "—".into(),
            })
            .collect()
    }

    fn folder(name: &str) -> (std::path::PathBuf, State) {
        let d = std::env::temp_dir().join(format!("arbor-menu-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for f in ["a.txt", "b.txt"] {
            std::fs::write(d.join(f), f).unwrap();
        }
        let mut s = state_in(&d);
        settle_all(&mut s);
        (d, s)
    }

    fn press() -> PointerPress {
        PointerPress { x: 300.0, y: 200.0 }
    }

    /// The same items the native menus show, from the same table; what can't run is off.
    #[test]
    fn it_lists_the_native_menus_items() {
        let (d, mut s) = folder("items");
        let mut tx = Tx::default();
        s.open_menu(s.menu_items(true), press(), &mut tx);
        let m = s.menu.as_ref().unwrap();
        assert_eq!((m.x, m.y, m.sel), (300.0, 200.0, None));
        assert_eq!(
            titles(m),
            [
                "Open",
                crate::commands::REVEAL,
                "—",
                "Rename…",
                "Duplicate",
                "Move to Trash",
                "—",
                "Copy",
                "Cut",
                "Paste",
                "Copy Path",
                "—",
                "New Folder",
                "Undo (off)"
            ]
        );
        assert!(m.items.iter().any(|it| matches!(it, Item::Cmd { cmd: Cmd::Copy, keys, .. } if *keys == s.keymap.label(Cmd::Copy))));
        // On empty space, what needs a row under the pointer is off.
        s.open_menu(s.menu_items(false), press(), &mut tx);
        assert!(titles(s.menu.as_ref().unwrap()).contains(&"Move to Trash (off)".to_string()));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Keyboard: ↓ from nothing highlights the first item, ↑ wraps, disabled rows and
    /// separators are skipped, a letter jumps, Enter runs, Esc closes; other keys stay in it.
    #[test]
    fn it_is_driven_by_the_keyboard() {
        let (d, mut s) = folder("keys");
        let mut tx = Tx::default();
        s.cur_mut().select(0);
        s.open_menu(s.menu_items(true), press(), &mut tx);
        let sel = |s: &State| s.menu.as_ref().and_then(|m| m.sel);
        assert!(s.on_key(&key(Key::Down), &mut tx));
        assert_eq!(sel(&s), Some(0), "Open");
        assert!(s.on_key(&key(Key::Up), &mut tx));
        assert_eq!(sel(&s), Some(12), "wrapped to New Folder, past the disabled Undo");
        assert!(s.on_key(&key(Key::Down), &mut tx));
        assert_eq!(sel(&s), Some(0));
        assert!(s.on_key(&key(Key::Down), &mut tx));
        assert!(s.on_key(&key(Key::Down), &mut tx));
        assert_eq!(sel(&s), Some(3), "past the separator to Rename");
        assert!(s.on_key(&key(Key::Text("c".into())), &mut tx));
        assert_eq!(sel(&s), Some(7), "Copy");
        assert!(s.on_key(&key(Key::Text("c".into())), &mut tx));
        assert_eq!(sel(&s), Some(8), "then Cut");
        assert!(s.on_key(&key(Key::Text("C".into())), &mut tx));
        assert_eq!(sel(&s), Some(10), "then Copy Path, case aside");
        assert!(s.on_key(&key(Key::End), &mut tx));
        assert_eq!(sel(&s), Some(12));
        let picked = s.cur().sel;
        assert!(s.on_key(&key(Key::PageDown), &mut tx), "other keys stay in the menu");
        assert_eq!(s.cur().sel, picked);
        assert!(s.on_key(&key(Key::Escape), &mut tx));
        assert!(s.menu.is_none());
        // Enter runs the highlight: Duplicate.
        s.open_menu(s.menu_items(true), press(), &mut tx);
        s.menu.as_mut().unwrap().sel = Some(4);
        assert!(s.on_key(&key(Key::Enter), &mut tx));
        assert!(s.menu.is_none() && s.job.is_some(), "Duplicate started");
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while s.job.is_some() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(2));
            s.poll(&mut Tx::default());
        }
        assert!(d.join("a copy.txt").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A click on a row that can't run does nothing but close.
    #[test]
    fn a_disabled_row_does_nothing() {
        let (d, mut s) = folder("off");
        let mut tx = Tx::default();
        s.open_menu(s.menu_items(true), press(), &mut tx);
        s.menu_pick(13, &mut tx);
        assert!(s.menu.is_none() && s.job.is_none() && s.note.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// It opens at the pointer, flips left or up at the window's edges, and stays inside.
    #[test]
    fn it_opens_at_the_pointer_and_stays_in_the_window() {
        use crate::view::menu_origin;
        let win = Some([1000.0, 700.0]);
        assert_eq!(menu_origin(300.0, 200.0, 250.0, 300.0, win), (300.0, 200.0));
        assert_eq!(menu_origin(900.0, 200.0, 250.0, 300.0, win), (650.0, 200.0), "flipped left");
        assert_eq!(menu_origin(300.0, 600.0, 250.0, 300.0, win), (300.0, 300.0), "flipped up");
        assert_eq!(menu_origin(100.0, 650.0, 250.0, 680.0, win), (100.0, 20.0), "too tall to flip: kept inside");
        assert_eq!(menu_origin(300.0, 200.0, 250.0, 300.0, None), (300.0, 200.0), "before the first layout");
    }
}
