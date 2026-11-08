//! The command palette (⌘K, or ⇧⌘P): a field over the top of the window and every command
//! ranked against what's typed (the titles, and the ids once the query has a `.`), each with its
//! shortcut. It costs nothing until opened: the state is an `Option`, and the ranking runs only
//! while it's open (about 50 short strings, microseconds), straight from the command table, so
//! there's nothing to keep in sync. Only the rows in view are built.
//!
//! Commands that can't run right now (Rename with nothing selected) are shown dimmed rather than
//! hidden: people use the palette to find out what Arbor can do and which key does it, and a
//! command that vanishes reads as one that doesn't exist. Choosing one says why it can't run.
//! The cursor starts on the best match that can run.

use std::borrow::Cow;

use rune::{Key, KeyEvent, TextEdit, Tx};

use crate::app::State;
use crate::commands::{Cmd, score};

/// Rows ↑↓ skip with Page Up / Page Down.
const PAGE: usize = 8;

pub struct Palette {
    pub edit: TextEdit,
    /// The cursor row, valid while the query is still `sel_for`; a new query starts it over.
    sel: usize,
    sel_for: String,
    /// Why the chosen command can't run, shown under the list while the query is `sel_for`.
    why: Option<&'static str>,
}

/// One row: a command that matches the query.
pub struct Hit {
    pub cmd: Cmd,
    pub title: Cow<'static, str>,
    pub keys: String,
    pub enabled: bool,
}

impl State {
    pub fn open_palette(&mut self) {
        let mut edit = TextEdit::new("");
        edit.focused = true;
        self.palette = Some(Palette { edit, sel: 0, sel_for: String::new(), why: None });
    }

    pub fn close_palette(&mut self, tx: &mut Tx) {
        if self.palette.take().is_some() {
            tx.blur();
        }
    }

    /// The commands matching the query, best first (in table order with no query). Palette
    /// itself and sidebar places that don't exist are left out.
    pub fn palette_hits(&self) -> Vec<Hit> {
        let Some(p) = &self.palette else { return Vec::new() };
        let q = p.edit.text.trim();
        let mut v: Vec<(i32, usize, Hit)> = Cmd::ALL
            .iter()
            .enumerate()
            .filter(|&(_, &cmd)| cmd != Cmd::Palette && cmd.place().is_none_or(|i| i < self.places.len()))
            .filter_map(|(i, &cmd)| {
                let title = self.title(cmd);
                // Ids ("file.trash", from the settings file) match once the query looks like one.
                let by_id = if q.contains(['.', '_']) { score(q, cmd.id()) } else { None };
                let s = [score(q, &title), by_id].into_iter().flatten().max()?;
                let enabled = self.unavailable(cmd).is_none();
                Some((s, i, Hit { cmd, title, keys: self.keymap.label(cmd), enabled }))
            })
            .collect();
        if !q.is_empty() {
            v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        v.into_iter().map(|(_, _, h)| h).collect()
    }

    /// The cursor row in `hits`.
    pub fn palette_sel(&self, hits: &[Hit]) -> usize {
        let Some(p) = &self.palette else { return 0 };
        if p.sel_for == p.edit.text { p.sel.min(hits.len().saturating_sub(1)) } else { hits.iter().position(|h| h.enabled).unwrap_or(0) }
    }

    /// Why the chosen command couldn't run, if that's still the latest news.
    pub fn palette_why(&self) -> Option<&'static str> {
        self.palette.as_ref().filter(|p| p.sel_for == p.edit.text).and_then(|p| p.why)
    }

    /// The field's binding. After the palette closes, the runtime's late writes (focus going
    /// away) land in a scratch field.
    pub fn palette_edit(&mut self) -> &mut TextEdit {
        match &mut self.palette {
            Some(p) => &mut p.edit,
            None => &mut self.palette_sink,
        }
    }

    /// The field's keys, before it edits: ↑↓ and Page Up/Down move, Enter runs, Esc closes.
    pub fn palette_field_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        tx.instant();
        let m = ev.mods;
        if m.cmd || m.ctrl || m.alt {
            return false;
        }
        match ev.key {
            Key::Up => self.palette_move(-1, tx),
            Key::Down => self.palette_move(1, tx),
            Key::PageUp => self.palette_move(-(PAGE as isize), tx),
            Key::PageDown => self.palette_move(PAGE as isize, tx),
            Key::Enter => {
                let hits = self.palette_hits();
                if let Some(h) = hits.get(self.palette_sel(&hits)) {
                    self.palette_pick(h.cmd, tx);
                }
            }
            Key::Escape => self.close_palette(tx),
            _ => return false,
        }
        true
    }

    /// The window's keys while the palette is open: what the field passed on, or every key if
    /// the field lost focus (a click on the panel). The palette's own chord closes it; nothing
    /// else reaches the folder behind it.
    pub fn palette_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        tx.instant();
        match self.keymap.find(|c| c.matches_tx(ev, tx)) {
            Some(Cmd::Palette) => {
                self.close_palette(tx);
                return true;
            }
            // Pastes into the field (a focused field takes the clipboard).
            Some(Cmd::Paste) => {
                tx.request_paste();
                return true;
            }
            _ => {}
        }
        if self.palette_field_key(ev, tx) {
            return true;
        }
        if let (Key::Text(t), Some(p)) = (&ev.key, &mut self.palette)
            && !ev.mods.cmd
            && !ev.mods.ctrl
        {
            p.edit.insert(t);
            p.edit.focused = true;
        }
        true
    }

    fn palette_move(&mut self, by: isize, tx: &mut Tx) {
        let hits = self.palette_hits();
        if hits.is_empty() {
            return;
        }
        let to = (self.palette_sel(&hits) as isize + by).clamp(0, hits.len() as isize - 1) as usize;
        if let Some(p) = &mut self.palette {
            p.sel = to;
            p.sel_for = p.edit.text.clone();
            p.why = None;
        }
        tx.scroll_into_view_in("palette-list", row_key(hits[to].cmd));
    }

    /// Enter or a click on a row: run it and close, or say why it can't run.
    pub fn palette_pick(&mut self, cmd: Cmd, tx: &mut Tx) {
        tx.instant();
        if let Some(why) = self.unavailable(cmd) {
            let hits = self.palette_hits();
            if let Some(p) = &mut self.palette {
                p.sel = hits.iter().position(|h| h.cmd == cmd).unwrap_or(0);
                p.sel_for = p.edit.text.clone();
                p.why = Some(why);
            }
            return;
        }
        self.close_palette(tx);
        self.run(cmd, tx);
    }
}

/// A row's key in the palette's list.
pub fn row_key(cmd: Cmd) -> String {
    format!("pc-{}", cmd.id())
}
