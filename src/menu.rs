//! The context menu where Rune has no native one (Linux, see RUNE-GAPS.md): a small card in the
//! window at the pointer, with the same items as the native menus on macOS and Windows
//! (`State::menu_items`), drawn by `menu_view` in `view/overlays.rs`.
//!
//! As a native menu does: nothing is highlighted when it opens; ↑ ↓ move (skipping separators
//! and what can't run), Home and End go to the ends, a letter goes to the next item starting
//! with it, Enter or Space runs the highlighted item, and Esc or a click outside closes it. It
//! takes every key while open. Nothing animates, and it costs nothing while closed.

use rune::{Key, KeyEvent, PointerPress, Tx};

use crate::app::State;
use crate::commands::Cmd;

/// What a menu row does: a command, something to do with a sidebar item, or opening the
/// selection with one of the apps Open With listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Cmd(Cmd),
    /// Sidebar item `usize` (favorites first, then volumes).
    Place(PlaceAct, usize),
    /// App `usize` of the list the menu was built with.
    OpenWith(usize),
}

/// What a sidebar item's menu can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceAct {
    Open,
    NewTab,
    NewWindow,
    Remove,
    Up,
    Down,
    Eject,
    AddToSidebar,
}

const PLACE_ACTS: [(PlaceAct, &str); 8] = [
    (PlaceAct::Open, "open"),
    (PlaceAct::NewTab, "tab"),
    (PlaceAct::NewWindow, "window"),
    (PlaceAct::Remove, "remove"),
    (PlaceAct::Up, "up"),
    (PlaceAct::Down, "down"),
    (PlaceAct::Eject, "eject"),
    (PlaceAct::AddToSidebar, "add"),
];

impl Act {
    /// The id a native menu item carries: a command's own id, `side.remove:3`, `openwith:2`.
    pub fn id(self) -> String {
        match self {
            Act::Cmd(c) => c.id().to_string(),
            Act::Place(a, i) => format!("side.{}:{i}", PLACE_ACTS.iter().find(|(p, _)| *p == a).map_or("open", |(_, n)| n)),
            Act::OpenWith(i) => format!("openwith:{i}"),
        }
    }

    pub fn from_id(id: &str) -> Option<Act> {
        if let Some(i) = id.strip_prefix("openwith:") {
            return i.parse().ok().map(Act::OpenWith);
        }
        if let Some((name, i)) = id.strip_prefix("side.").and_then(|r| r.split_once(':')) {
            let a = PLACE_ACTS.iter().find(|(_, n)| *n == name)?.0;
            return i.parse().ok().map(|i| Act::Place(a, i));
        }
        Cmd::from_id(id).map(Act::Cmd)
    }

    /// The command it runs, if it's one.
    pub fn cmd(self) -> Option<Cmd> {
        match self {
            Act::Cmd(c) => Some(c),
            _ => None,
        }
    }
}

/// One row: what it does (its title for what's selected, whether it can run, its keys as this
/// platform draws them), or a separator.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// `checked`: a toggle (a column, Folders First), and whether it's on.
    Act {
        act: Act,
        title: String,
        on: bool,
        keys: String,
        checked: Option<bool>,
    },
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
        self.items.iter().enumerate().filter(|(_, it)| matches!(it, Item::Act { on: true, .. })).map(|(i, _)| i)
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
        let starts = |i: usize| matches!(&self.items[i], Item::Act { title, .. } if title.to_lowercase().starts_with(c));
        let live: Vec<usize> = self.live().filter(|&i| starts(i)).collect();
        let after = self.sel.and_then(|s| live.iter().copied().find(|&i| i > s));
        if let Some(i) = after.or(live.first().copied()) {
            self.sel = Some(i);
        }
    }
}

impl State {
    /// Opens the in-window menu at `press` with `items` (from `State::menu_items`).
    pub(crate) fn open_menu(&mut self, items: Vec<Option<(Act, String, bool)>>, press: PointerPress, tx: &mut Tx) {
        self.close_overlays(tx);
        let items = items
            .into_iter()
            .map(|it| match it {
                Some((act, title, on)) => Item::Act {
                    act,
                    title,
                    on,
                    keys: act.cmd().map(|c| self.keymap.label(c)).unwrap_or_default(),
                    checked: act.cmd().and_then(|c| self.checked(c)),
                },
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
        if let Some(Item::Act { act, on: true, .. }) = m.items.get(i) {
            self.run_act(*act, tx);
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
                Item::Act { title, on, .. } => format!("{title}{}", if *on { "" } else { " (off)" }),
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
                "Open With…",
                crate::commands::REVEAL,
                "—",
                "Get Info",
                "Rename…",
                "Duplicate",
                "Make Symbolic Link",
                "Compress “a.txt”",
                "Move to Trash",
                "—",
                "Copy",
                "Cut",
                "Paste",
                "Copy Path",
                "—",
                "Add to Sidebar (off)",
                "New Folder",
                "New File",
                "Undo (off)"
            ]
        );
        assert!(
            m.items.iter().any(|it| matches!(it, Item::Act { act: Act::Cmd(Cmd::Copy), keys, .. } if *keys == s.keymap.label(Cmd::Copy)))
        );
        // On empty space, what needs a row under the pointer is off.
        s.open_menu(s.menu_items(false), press(), &mut tx);
        let t = titles(s.menu.as_ref().unwrap());
        assert!(t.contains(&"Move to Trash (off)".to_string()) && t.contains(&"Open With… (off)".to_string()));
        assert!(t.contains(&"Add to Sidebar".to_string()), "the folder here: {t:?}");
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
        assert_eq!(sel(&s), Some(18), "wrapped to New File, past the disabled Undo");
        assert!(s.on_key(&key(Key::Down), &mut tx));
        assert_eq!(sel(&s), Some(0));
        for _ in 0..3 {
            assert!(s.on_key(&key(Key::Down), &mut tx));
        }
        assert_eq!(sel(&s), Some(4), "past the separator to Get Info");
        assert!(s.on_key(&key(Key::Text("c".into())), &mut tx));
        assert_eq!(sel(&s), Some(8), "Compress");
        assert!(s.on_key(&key(Key::Text("c".into())), &mut tx));
        assert_eq!(sel(&s), Some(11), "then Copy");
        assert!(s.on_key(&key(Key::Text("c".into())), &mut tx));
        assert_eq!(sel(&s), Some(12), "then Cut");
        assert!(s.on_key(&key(Key::Text("C".into())), &mut tx));
        assert_eq!(sel(&s), Some(14), "then Copy Path, case aside");
        assert!(s.on_key(&key(Key::End), &mut tx));
        assert_eq!(sel(&s), Some(18));
        let picked = s.cur().sel;
        assert!(s.on_key(&key(Key::PageDown), &mut tx), "other keys stay in the menu");
        assert_eq!(s.cur().sel, picked);
        assert!(s.on_key(&key(Key::Escape), &mut tx));
        assert!(s.menu.is_none());
        // Enter runs the highlight: Duplicate.
        s.open_menu(s.menu_items(true), press(), &mut tx);
        s.menu.as_mut().unwrap().sel = Some(6);
        assert!(s.on_key(&key(Key::Enter), &mut tx));
        assert!(s.menu.is_none() && s.owns_job(), "Duplicate started");
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while s.owns_job() && std::time::Instant::now() < until {
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
        s.menu_pick(19, &mut tx);
        assert!(s.menu.is_none() && !s.owns_job() && s.note.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn act_ids_round_trip() {
        for a in [Act::Cmd(Cmd::Copy), Act::Place(PlaceAct::Remove, 3), Act::Place(PlaceAct::Eject, 12), Act::OpenWith(2)] {
            assert_eq!(Act::from_id(&a.id()), Some(a), "{}", a.id());
        }
        assert_eq!(Act::from_id("side.nope:1"), None);
        assert_eq!(Act::from_id("openwith:x"), None);
    }

    /// A favorite's menu: open, open in a new tab or window, remove and move (the ends can't move past
    /// themselves); picking runs it.
    #[test]
    fn a_favorites_menu_edits_the_sidebar() {
        let (d, mut s) = folder("side");
        let mut tx = Tx::default();
        s.places = crate::sidebar::named(vec![d.join("x"), d.clone(), d.join("y")]);
        s.open_menu(s.sidebar_menu_items(0), press(), &mut tx);
        assert_eq!(
            titles(s.menu.as_ref().unwrap()),
            ["Open", "Open in New Tab", "Open in New Window", "—", "Remove from Sidebar", "Move Up (off)", "Move Down"]
        );
        s.menu_pick(6, &mut tx);
        assert_eq!(s.places[1].1, d.join("x"), "moved down");
        s.open_menu(s.sidebar_menu_items(1), press(), &mut tx);
        s.menu_pick(4, &mut tx);
        assert_eq!(s.places.iter().map(|p| p.1.clone()).collect::<Vec<_>>(), [d.clone(), d.join("y")]);
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
