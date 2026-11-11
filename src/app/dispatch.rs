//! Keys and commands: a key goes to the open overlay or field first, then through the keymap to
//! a command; `run` asks first for what can't be undone, and `run_cmd` does each command
//! (`unavailable` says when one can't run). Both match on `Cmd` exhaustively.

use super::*;
use crate::grid::ViewMode;

impl State {
    pub fn on_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        // A key after the leftover note came: it's been seen.
        self.forget_leftovers();
        let cmd = crate::draglog::on().then(|| self.keymap.find(|c| c.matches_tx(ev, tx))).flatten();
        let handled = self.key(ev, tx);
        if handled {
            self.flush_columns(tx);
            self.sync_preview();
            self.flush_columns(tx);
        }
        crate::draglog::log(|| {
            let m = ev.mods;
            let ran = cmd.map_or("unhandled".to_string(), |c| format!("{c:?}"));
            format!("key {:?} cmd={} shift={} alt={} -> {}", ev.key, m.cmd, m.shift, m.alt, if handled { ran } else { "passed on".into() })
        });
        handled
    }

    pub(super) fn key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        tx.instant();
        // The confirmation dialog is modal: it has every key.
        if self.asking.is_some() {
            return self.ask_key(ev, tx);
        }
        if self.menu.is_some() {
            return self.menu_key(ev, tx);
        }
        // While renaming, the focused field has the keys (its own `rename_field_key` takes Esc).
        // Keys get here when it isn't focused (a click elsewhere blurred it): Esc still cancels
        // and Enter still commits, so the field never stays on screen with no way out.
        if self.rename.is_some() {
            match ev.key {
                Key::Escape => self.cancel_rename(tx),
                Key::Enter => self.commit_rename(tx),
                _ => {}
            }
            return true;
        }
        if self.palette.is_some() {
            return self.palette_key(ev, tx);
        }
        if self.jump.is_some() {
            return self.jump_key(ev, tx);
        }
        if self.pick.is_some() {
            return self.pick_key(ev, tx);
        }
        if self.batch.is_some() {
            return self.batch_key(ev, tx);
        }
        if self.open_with.is_some() {
            return self.open_with_key(ev, tx);
        }
        // In a search with the results focused, ⌫ edits the query (as typing does, below).
        let m = ev.mods;
        if self.cur().searching() && ev.key == Key::Backspace && !m.cmd && !m.ctrl && !m.alt {
            self.type_in_search(self.active, None, tx);
            return true;
        }
        // An open go-to field that Backspace emptied keeps Backspace and Enter: held down to
        // clear the field, Backspace would otherwise go on to the parent folder.
        let f = self.cur();
        if f.goto && f.filter.is_empty() && matches!(ev.key, Key::Backspace | Key::Enter) && !m.cmd && !m.ctrl {
            return true;
        }
        if self.edit_filter(ev, tx) {
            return true;
        }
        // In a grid view (icons, tiles), ← and → move along the grid (⇧ extends); ⌘↑ and ⌘↓
        // still go up and open, as Finder's icon view does.
        if self.cur().view.is_grid() && matches!(ev.key, Key::Left | Key::Right) && !m.cmd && !m.ctrl && !m.alt {
            let dx = if ev.key == Key::Left { -1 } else { 1 };
            self.cur_mut().step_grid(dx, 0, m.shift, tx);
            return true;
        }
        if let Some(mut cmd) = self.keymap.find(|c| c.matches_tx(ev, tx)) {
            // Esc closes the preview before it clears anything (the palette and a rename have
            // had it first, above); not the column view's, which follows the selection.
            if cmd == Cmd::Clear && self.preview.is_some() && !self.col_preview {
                cmd = Cmd::ClosePreview;
            }
            // And Get Info's panel, which takes its place.
            if cmd == Cmd::Clear && self.info.is_some() {
                cmd = Cmd::Info;
            }
            // Then a running file operation; then in a search, it stops the walk or leaves.
            if cmd == Cmd::Clear && !self.owns_job() && self.cur().searching() {
                self.search_escape(self.active, tx);
                return true;
            }
            if let Some(why) = self.unavailable(cmd) {
                // Closing the last tab does nothing, the same on every platform: passed on, ⌘W
                // would close the window on macOS (Rune's standard Close item).
                if cmd == Cmd::CloseTab {
                    self.note = Some(why.into());
                    return true;
                }
                // A command that would start a file operation while one runs (in this window or
                // another) says why, rather than doing nothing silently.
                if why == BUSY || why == BUSY_ELSEWHERE {
                    self.note = Some(why.into());
                    return true;
                }
                // Any other command that can't run lets the key go on (Esc with nothing to clear).
                return false;
            }
            self.run(cmd, tx);
            return true;
        }
        if self.cur().searching() {
            if let Key::Text(t) = &ev.key
                && !ev.mods.cmd
                && !ev.mods.ctrl
            {
                self.type_in_search(self.active, Some(t), tx);
                return true;
            }
            return false;
        }
        let f = self.cur_mut();
        match &ev.key {
            // Space with nothing typed (and Toggle Preview unbound from it) types nothing: a
            // filter never starts with a space.
            Key::Text(t) if t == " " && f.filter.is_empty() => false,
            Key::Text(t) if !ev.mods.cmd && !ev.mods.ctrl => {
                f.type_filter(t);
                f.reveal(tx);
                true
            }
            _ => false,
        }
    }

    /// While the filter has text, typing, ⌫ and ⌥⌫ edit it, and in go-to mode Enter goes and
    /// Tab completes (`complete.rs`), before any command sees those keys.
    pub(super) fn edit_filter(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        let f = self.cur_mut();
        if f.filter.is_empty() {
            return false;
        }
        let m = ev.mods;
        match &ev.key {
            Key::Backspace if m.alt && !m.cmd && !m.ctrl => {
                f.filter.clear();
                f.refilter(None);
            }
            Key::Backspace if !m.cmd && !m.ctrl && !m.alt => {
                f.filter.pop();
                let keep = f.cursor_name();
                f.refilter(keep.as_deref());
                f.reveal(tx);
            }
            Key::Tab if !m.cmd && !m.ctrl && !m.alt && f.path_mode() => self.complete_tab(m.shift, tx),
            Key::Enter if !m.cmd && !m.ctrl && !m.alt && f.path_mode() => self.submit_path(),
            Key::Text(t) if !m.cmd && !m.ctrl => {
                f.type_filter(t);
                f.reveal(tx);
            }
            _ => return false,
        }
        true
    }

    /// Why `cmd` can't run right now, or `None` when it can. Keys for an unavailable command
    /// pass through; the palette shows it dimmed and says this when it's chosen. Every command
    /// is listed, so a new one has to decide.
    pub fn unavailable(&self, cmd: Cmd) -> Option<&'static str> {
        use Cmd::*;
        let f = self.cur();
        let nothing = f.selected().is_none();
        let empty = f.visible.is_empty();
        // File operations run one at a time, in the whole app (`State::start`).
        let busy = self.busy();
        let busy_why = self.busy_why();
        let pane = &self.panes[self.active];
        let when = |cond: bool, why: &'static str| cond.then_some(why);
        let searching = f.searching();
        // An item an inline rename is still renaming: nothing acts on it until that lands.
        let renaming = busy && f.targets().iter().any(|e| e.pending.is_some());
        let renaming = when(renaming, "Wait for the rename to finish");
        match cmd {
            // Start a file operation.
            Trash | Delete => when(busy, busy_why).or(when(nothing, "Select an item first")),
            EmptyTrash => when(busy, busy_why),
            PutBack => when(cfg!(target_os = "macos"), "macOS has no public way for apps to put items back: use Put Back in Finder")
                .or(when(cfg!(windows), "Restore items from the Recycle Bin in Explorer: Windows doesn't let Arbor list it"))
                .or(when(busy, busy_why))
                .or(when(searching, "Leave the search first (Esc)"))
                .or(when(!crate::trash::contents::can_put_back_in(&f.cwd), "Put Back works on items in the Trash: Go to Trash first"))
                .or(when(nothing, "Select an item first")),
            GoTrash => when(self.trash_place.is_none(), "The Recycle Bin is a shell folder Arbor can't show: open it in Explorer"),
            // A duplicate goes beside its original, so all of them must be in one folder.
            Duplicate => when(busy, busy_why).or(when(nothing, "Select an item first")).or(when(
                searching && f.target_paths().iter().map(|p| p.parent()).collect::<std::collections::HashSet<_>>().len() > 1,
                "Duplicate items from one folder at a time",
            )),
            CopyToOther | MoveToOther => when(!self.split(), NOT_SPLIT)
                .or(when(busy, busy_why))
                .or(when(nothing, "Select an item first"))
                .or(when(self.other().is_some_and(Folder::searching), IN_OTHER_SEARCH))
                .or(when(self.other().is_some_and(|o| o.shows(&f.cwd, f.dir_id)), "Both panes show the same folder")),
            // Paste always shows as available otherwise: Rune can't ask the clipboard what it
            // holds without reading it (RUNE-GAPS.md). Pasting nothing says so.
            Paste | PasteMove => when(busy, busy_why).or(when(searching, "Leave the search (Esc) to paste into this folder")),
            Undo => when(busy, busy_why).or(when(self.shared.undo.borrow().is_empty(), "Nothing to undo")),
            Redo => when(busy, busy_why).or(when(self.shared.redo.borrow().is_empty(), "Nothing to redo")),
            Cancel => when(!busy, "No file operation is running")
                .or(when(!self.owns_job(), "The file operation is running in another window: cancel it there")),
            Open | Copy | Cut | Rename | BatchRename => when(nothing, "Select an item first").or(renaming),
            Into => when(!f.selected().is_some_and(|e| e.is_dir), "Select a folder first").or(renaming),
            Parent => when(searching, "Leave the search first (Esc)").or(when(f.cwd.parent().is_none(), "Already at the top")),
            // In a search, Back leaves it.
            Back => when(!searching && f.back.is_empty(), "Nothing to go back to"),
            Forward => when(f.fwd.is_empty(), "Nothing to go forward to"),
            Clear => when(
                !self.owns_job() && !searching && f.filter.is_empty() && !f.goto && self.note.is_none() && f.marked.is_empty(),
                "Nothing to clear",
            ),
            Up | Down | PageUp | PageDown | First | Last | ExtendUp | ExtendDown | ExtendPageUp | ExtendPageDown | ExtendFirst
            | ExtendLast | SelectAll | SelectPattern | InvertSelection => when(empty, "Nothing to select"),
            SelectKind => when(nothing, "Select an item first"),
            Jump => when(!self.shared.history.borrow().enabled, "Jump is turned off in the settings (jump.enabled)"),
            SortName => when(f.sort.key == SortKey::Name, "Already sorted by name"),
            SortKind => when(f.sort.key == SortKey::Kind, "Already sorted by kind"),
            SortSize => when(f.sort.key == SortKey::Size, "Already sorted by size"),
            SortModified => when(f.sort.key == SortKey::Modified, "Already sorted by date modified"),
            // The last tab of a window closes the window, while there are others.
            CloseTab => when(!self.split() && pane.tabs.len() == 1 && self.windows() < 2, "This is the only tab"),
            OpenInNewWindow => when(self.folder_to_open().is_none(), "Select one folder first"),
            NextTab | PrevTab => when(pane.tabs.len() == 1, "There's only one tab here"),
            ReopenTab => when(self.closed.is_empty(), "No closed tabs to reopen"),
            MoveTabLeft => when(pane.tabs.len() == 1, "There's only one tab here").or(when(pane.tab == 0, "This is already the first tab")),
            MoveTabRight => when(pane.tabs.len() == 1, "There's only one tab here")
                .or(when(pane.tab + 1 == pane.tabs.len(), "This is already the last tab")),
            MoveTabOther => when(!self.split(), NOT_SPLIT),
            OtherPane => when(!self.split(), NOT_SPLIT),
            // Closing always works; opening needs something to show.
            TogglePreview => when(self.preview.is_none() && nothing, "Select an item first"),
            ClosePreview => when(self.preview.is_none(), "The preview isn't open"),
            AsList => when(f.view == ViewMode::List, "Already showing as a list"),
            AsIcons => when(f.view == ViewMode::Icons, "Already showing as icons"),
            AsColumns => when(f.view == ViewMode::Columns, "Already showing as columns"),
            AsTiles => when(f.view == ViewMode::Tiles, "Already showing as tiles"),
            LargerIcons => {
                when(f.view != ViewMode::Icons, ICONS_ONLY).or(when(self.icon_size.step(1).is_none(), "The icons are as large as they go"))
            }
            SmallerIcons => {
                when(f.view != ViewMode::Icons, ICONS_ONLY).or(when(self.icon_size.step(-1).is_none(), "The icons are as small as they go"))
            }
            Place1 | Place2 | Place3 | Place4 | Place5 | Place6 | Place7 | Place8 | Place9 => {
                when(cmd.place().is_some_and(|i| i >= self.place_count()), "No sidebar place there")
            }
            OpenWith => when(!f.targets().iter().any(|e| !e.is_dir), "Select a file first").or(renaming),
            SidebarRemove => when(self.favorite_here().is_none(), "This folder isn't in the sidebar"),
            Eject => when(self.ejecting.is_some(), "Already ejecting a volume")
                .or(when(busy, "Wait for the file operation to finish, then eject"))
                .or(when(self.volume_here().is_none(), "This folder isn't on a volume that can be ejected")),
            // Always available.
            NewFolder => when(busy, busy_why).or(when(searching, "Leave the search (Esc) to make a folder here")),
            NewFile => when(busy, busy_why).or(when(searching, "Leave the search (Esc) to make a file here")),
            MakeLink => when(busy, busy_why).or(when(nothing, "Select an item first")).or(renaming),
            ShowOriginal => when(!f.selected().is_some_and(|e| e.is_link), "Select a symbolic link first").or(renaming),
            MakeDefault | RestoreDefault => when(self.handler_job.is_pending(), "Already changing the default for folders"),
            Reveal | CopyPath | Info | SidebarAdd => renaming,
            GoTo | Reload | Palette | OpenSettings | ReloadSettings | Hidden | SortReverse | NewTab | NewWindow | Split | ClearJump
            | Search => None,
        }
    }

    /// The title the palette shows, with what's here filled in.
    pub fn title(&self, cmd: Cmd) -> Cow<'static, str> {
        match cmd {
            Cmd::Hidden if self.show_hidden => "Hide Hidden Files".into(),
            Cmd::Hidden => "Show Hidden Files".into(),
            Cmd::Split if self.split() => "Close Split View".into(),
            Cmd::Split => "Split View".into(),
            Cmd::TogglePreview if self.preview.is_some() => "Close Preview".into(),
            Cmd::TogglePreview => "Quick Preview".into(),
            Cmd::Rename if self.cur().marked.len() > 1 => Cmd::BatchRename.title().into(),
            Cmd::Info if self.info.is_some() => "Close Get Info".into(),
            Cmd::Eject => match self.volume_here().and_then(|v| self.volumes.get(v)) {
                Some(v) => format!("Eject “{}”", v.name).into(),
                None => cmd.title().into(),
            },
            _ => match cmd.place().and_then(|i| self.place_at(i)) {
                Some((name, _)) => format!("Go to {name}").into(),
                None => cmd.title().into(),
            },
        }
    }

    /// Runs `cmd`, from a key, the palette or the context menu. Check `unavailable` first.
    pub fn run(&mut self, cmd: Cmd, tx: &mut Tx) {
        // What can't be undone, or changes the system, asks first; only the dialog's button runs
        // it (`run_permanent`).
        if cmd.asks() {
            self.ask(cmd, tx);
            return;
        }
        self.run_cmd(cmd, tx);
        // A column made active lands first, so the sync sees it in place.
        self.flush_columns(tx);
        self.sync_preview();
        self.flush_columns(tx);
    }

    /// Runs a permanent command, confirmed in the dialog.
    pub(crate) fn run_permanent(&mut self, cmd: Cmd, tx: &mut Tx) {
        self.run_cmd(cmd, tx);
        self.flush_columns(tx);
        self.sync_preview();
        self.flush_columns(tx);
    }

    pub(super) fn run_cmd(&mut self, cmd: Cmd, tx: &mut Tx) {
        use Cmd::*;
        let page = PAGE as isize;
        match cmd {
            Open => self.open_selected(),
            Into => {
                let f = self.cur_mut();
                f.select(f.sel);
                self.open_selected();
            }
            Parent => self.parent(),
            Back if self.cur().searching() => self.leave_search(self.active, tx),
            Back => self.go_back(),
            Forward => self.go_forward(),
            GoTo => {
                self.leave_search(self.active, tx);
                let f = self.cur_mut();
                let mut p = fs::pretty(&f.cwd);
                if !p.ends_with(std::path::MAIN_SEPARATOR) {
                    p.push(std::path::MAIN_SEPARATOR);
                }
                f.filter = p;
                f.goto = true;
                f.refilter(None);
            }
            Reload if self.cur().searching() => {
                self.refresh_volumes();
                self.restart_search(self.active, tx);
            }
            Reload => {
                // ⌘R also reads the sidebar's volumes again (one of the explicit refreshes),
                // and the column view's columns.
                self.refresh_volumes();
                if let Some(cols) = self.cur_mut().cols.as_mut() {
                    cols.reread(&[], true);
                }
                self.reload();
            }
            Reveal => self.reveal_selected(),
            OpenWith => self.open_with(tx),
            Info => self.toggle_info(),
            SidebarAdd => self.add_to_sidebar(),
            SidebarRemove => {
                if let Some(i) = self.favorite_here() {
                    self.remove_favorite(i);
                }
            }
            Eject => {
                if let Some(v) = self.volume_here().and_then(|v| self.volumes.get(v)).cloned() {
                    self.eject(v);
                }
            }
            Palette => {
                self.close_overlays(tx);
                self.open_palette();
            }
            Jump => self.open_jump(tx),
            Search => self.open_search(tx),
            ClearJump => self.clear_jump_history(),
            SelectPattern => self.open_pick(tx),
            InvertSelection => self.invert_selection(tx),
            SelectKind => self.select_kind(tx),
            OpenSettings => self.open_settings(),
            ReloadSettings => self.reload_settings(),
            MakeDefault => self.change_default(crate::handler::Change::Make),
            RestoreDefault => self.change_default(crate::handler::Change::Restore),
            Clear => {
                if self.owns_job() {
                    self.note = self.cancel_job();
                } else if self.cur().searching() {
                    self.search_escape(self.active, tx);
                } else {
                    self.note = None;
                    let f = self.cur_mut();
                    f.marked.clear();
                    let keep = f.cursor_name();
                    f.filter.clear();
                    f.goto = false;
                    f.refilter(keep.as_deref());
                    f.reveal(tx);
                }
            }
            Copy => self.copy_selected(false, tx),
            Cut => self.copy_selected(true, tx),
            // The clipboard answers a frame or more later, to this request alone: where it goes
            // and whether it moves are fixed now, by the pane it was asked in.
            Paste | PasteMove => {
                self.next_paste += 1;
                let (id, f) = (self.next_paste, self.cur());
                self.paste_reqs.push(PasteReq { id, dest: f.cwd.clone(), dest_id: f.dir_id, mv: cmd == PasteMove });
                tx.read_clipboard(move |s: &mut State, content, tx| {
                    tx.instant();
                    s.paste_reply(id, content);
                });
            }
            CopyPath => self.copy_path(tx),
            Duplicate => self.duplicate(),
            // Two or more: Batch Rename, as Finder does.
            Rename if self.cur().marked.len() > 1 => self.open_batch(tx),
            Rename => self.start_rename(),
            BatchRename => self.open_batch(tx),
            NewFolder => self.new_folder(),
            NewFile => self.new_file(),
            MakeLink => self.make_links(),
            ShowOriginal => self.show_original(),
            Trash => self.trash_selected(),
            // Runs from the dialog (`State::answer`), with the items it named.
            Delete => {}
            MoveTabLeft => self.move_tab_by(-1, tx),
            MoveTabRight => self.move_tab_by(1, tx),
            MoveTabOther => {
                let (ix, tab) = (self.active, self.panes[self.active].tab);
                let n = self.panes[1 - ix].tabs.len();
                self.move_tab_to_pane(ix, tab, 1 - ix, n, tx);
            }
            Undo => self.undo_last(),
            Redo => self.redo_last(),
            Cancel => {
                if self.owns_job() {
                    self.note = self.cancel_job();
                }
            }
            GoTrash => {
                if let Some(p) = self.trash_place.clone() {
                    self.leave_search(self.active, tx);
                    self.go_to(p);
                }
            }
            PutBack => self.put_back_selected(),
            EmptyTrash => self.empty_trash(),
            CopyToOther => self.send_to_other_pane(false),
            MoveToOther => self.send_to_other_pane(true),
            Up | Down | PageUp | PageDown | ExtendUp | ExtendDown | ExtendPageUp | ExtendPageDown if self.cur().view.is_grid() => {
                // A row at a time, or a page of rows (as many as a list page's height holds).
                let cell_h = self.cur().view.geom(self.icon_size, f32::MAX).map_or(24.0, |g| g.cell[1]);
                let rows = ((PAGE as f32 * 24.0) / cell_h).floor().max(1.0) as isize;
                let (dy, extend) = match cmd {
                    Up => (-1, false),
                    Down => (1, false),
                    PageUp => (-rows, false),
                    PageDown => (rows, false),
                    ExtendUp => (-1, true),
                    ExtendDown => (1, true),
                    ExtendPageUp => (-rows, true),
                    _ => (rows, true),
                };
                self.cur_mut().step_grid(0, dy, extend, tx);
            }
            Up => self.cur_mut().step(-1, false, tx),
            Down => self.cur_mut().step(1, false, tx),
            PageUp => self.cur_mut().step(-page, false, tx),
            PageDown => self.cur_mut().step(page, false, tx),
            First => self.cur_mut().step(isize::MIN / 2, false, tx),
            Last => self.cur_mut().step(isize::MAX / 2, false, tx),
            ExtendUp => self.cur_mut().step(-1, true, tx),
            ExtendDown => self.cur_mut().step(1, true, tx),
            ExtendPageUp => self.cur_mut().step(-page, true, tx),
            ExtendPageDown => self.cur_mut().step(page, true, tx),
            ExtendFirst => self.cur_mut().step(isize::MIN / 2, true, tx),
            ExtendLast => self.cur_mut().step(isize::MAX / 2, true, tx),
            SelectAll => self.cur_mut().select_all(),
            Hidden => {
                self.toggle_hidden();
                // Hidden folders are walked or skipped: searches start again.
                for ix in 0..self.panes.len() {
                    if self.panes[ix].f.searching() {
                        self.restart_search(ix, tx);
                    }
                }
            }
            SortName => self.sort_by(SortKey::Name, tx),
            SortKind => self.sort_by(SortKey::Kind, tx),
            SortSize => self.sort_by(SortKey::Size, tx),
            SortModified => self.sort_by(SortKey::Modified, tx),
            SortReverse => {
                let key = self.cur().sort.key;
                self.sort_by(key, tx);
            }
            NewTab => self.new_tab(tx),
            NewWindow => {
                let here = self.cur().cwd.clone();
                self.open_window(here);
            }
            OpenInNewWindow => {
                if let Some(p) = self.folder_to_open() {
                    self.open_window(p);
                }
            }
            // The last tab of the window, with other windows open: the window goes.
            CloseTab if !self.split() && self.panes[0].tabs.len() == 1 => tx.close_window(),
            CloseTab => {
                let (ix, tab) = (self.active, self.panes[self.active].tab);
                self.close_tab(ix, tab, tx);
            }
            NextTab => self.cycle_tab(1, tx),
            PrevTab => self.cycle_tab(-1, tx),
            ReopenTab => self.reopen_tab(tx),
            Split => self.toggle_split(tx),
            OtherPane => self.activate(1 - self.active, tx),
            TogglePreview => self.toggle_preview(),
            ClosePreview => self.close_preview(),
            AsList => self.set_view(ViewMode::List, tx),
            AsIcons => self.set_view(ViewMode::Icons, tx),
            AsColumns => self.set_view(ViewMode::Columns, tx),
            AsTiles => self.set_view(ViewMode::Tiles, tx),
            LargerIcons | SmallerIcons => {
                if let Some(size) = self.icon_size.step(if cmd == LargerIcons { 1 } else { -1 }) {
                    self.view_chosen = true;
                    self.icon_size = size;
                    self.cur().regrid(size);
                    self.cur().reveal(tx);
                }
            }
            Place1 | Place2 | Place3 | Place4 | Place5 | Place6 | Place7 | Place8 | Place9 => {
                if let Some(i) = cmd.place() {
                    self.go_place(i);
                }
            }
        }
    }

    pub(super) fn sort_by(&mut self, key: SortKey, tx: &mut Tx) {
        let f = self.cur_mut();
        f.set_sort(key);
        f.reveal(tx);
    }
}

/// Why the icon size commands can't run in the list view.
const ICONS_ONLY: &str = "Icon sizes are for the icon view (As Icons)";

impl State {
    /// As List, As Icons, As Columns or As Tiles, for the active tab.
    pub(crate) fn set_view(&mut self, mode: ViewMode, tx: &mut Tx) {
        self.view_chosen = true;
        self.band = None;
        let size = self.icon_size;
        let f = self.cur_mut();
        f.view = mode;
        // Another view: no thumbnails are decoded or kept for the old one (a grid asks again
        // for the cells it shows), and its rows are built again (the rows' element keeps its
        // key across views).
        f.tiles = None;
        f.grid_rows = None;
        f.tiles_for = None;
        f.bump();
        f.regrid(size);
        f.reveal(tx);
        // The columns (or none, leaving them), their reads and the preview column.
        self.sync_preview();
        self.flush_columns(tx);
    }
}

/// Asks a running operation to stop, and what the status bar says.
impl State {
    /// Esc or Cancel File Operation: stops the running operation, if it can be stopped.
    fn cancel_job(&self) -> Option<String> {
        let slot = self.shared.job.borrow();
        let job = &slot.as_ref()?.run;
        Some(if job.cancellable {
            job.cancel();
            "Cancelling…".into()
        } else {
            format!("{} can't be stopped once it has started", job.verb)
        })
    }
}
