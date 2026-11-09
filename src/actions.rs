//! File operations from the UI: clipboard, paste, duplicate, rename, new folder, trash, undo,
//! drag and drop, and the context menu. The work itself runs in `ops` on a worker thread.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;

use rune::{ClipboardContent, DragItem, PointerPress, PopupItem, PopupMenu, TextEdit, Tx};

use crate::app::{Clip, Nav, PasteReq, Rename, State};
use crate::commands::Cmd;
use crate::menu::Act;
use crate::ops::{self, Op};

impl State {
    // ---- clipboard --------------------------------------------------------------------------

    /// ⌘C / ⌘X: files on the system clipboard (Finder and Explorer paste them), paths as text.
    pub fn copy_selected(&mut self, cut: bool, tx: &mut Tx) {
        let paths = self.target_paths();
        if paths.is_empty() {
            return;
        }
        let text = paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
        tx.copy_rich(ClipboardContent::new(text).with_files(paths.clone()));
        let n = paths.len();
        self.note = Some(match (cut, n) {
            (false, 1) => format!("Copied {}", name(&paths[0])),
            (false, n) => format!("Copied {n} items"),
            (true, 1) => format!("Cut {} — paste to move it", name(&paths[0])),
            (true, n) => format!("Cut {n} items — paste to move them"),
        });
        let expect = self.cur().target_checks();
        self.clip = Some(Clip { paths, cut, expect });
    }

    /// ⌘⇧C: the selected path as text.
    pub fn copy_path(&mut self, tx: &mut Tx) {
        let paths = self.target_paths();
        let text = if paths.is_empty() {
            self.cur().cwd.display().to_string()
        } else {
            paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n")
        };
        self.note = Some(format!("Copied {text}"));
        tx.copy(text);
    }

    /// The root's paste handler, for a paste the OS started: into the active pane's folder.
    pub fn paste(&mut self, content: &ClipboardContent) {
        if let Some(why) = self.unavailable(Cmd::Paste) {
            self.note = Some(why.into());
            return;
        }
        self.paste_with(None, content);
    }

    /// The clipboard's answer to paste request `id` (`Cmd::Paste`).
    pub(crate) fn paste_reply(&mut self, id: u64, content: &ClipboardContent) {
        if let Some(i) = self.paste_reqs.iter().position(|r| r.id == id) {
            let req = self.paste_reqs.remove(i);
            self.paste_with(Some(req), content);
        }
    }

    /// Files from the clipboard (or absolute paths in its text) go into the folder the paste
    /// was asked for in, even if another pane is active by the time the clipboard answers (or
    /// that folder isn't on screen any more: it's where the user said, and the paste is
    /// undoable), but only while it's still that folder: one put in its place since is refused
    /// (`ops::start`). A paste of what Arbor itself cut, or ⌥⌘V, moves.
    fn paste_with(&mut self, req: Option<PasteReq>, content: &ClipboardContent) {
        let (dest, dest_id, move_requested) = match req {
            Some(PasteReq { dest, dest_id, mv, .. }) => (dest, dest_id, mv),
            None => (self.cur().cwd.clone(), self.cur().dir_id, false),
        };
        let mut files: Vec<PathBuf> = content.files().to_vec();
        if files.is_empty() {
            files = content
                .text()
                .lines()
                .map(str::trim)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute() && p.symlink_metadata().is_ok())
                .collect();
        }
        if files.is_empty() && content.is_empty() {
            files = self.clip.as_ref().map(|c| c.paths.clone()).unwrap_or_default();
        }
        if files.is_empty() {
            self.note = Some("Nothing to paste: copy some files first".into());
            return;
        }
        let ours = self.clip.as_ref().filter(|c| c.paths == files);
        let was_cut = ours.is_some_and(|c| c.cut);
        let expect = ours.map(|c| c.expect.clone()).unwrap_or_default();
        let op = if move_requested || was_cut { Op::Move { sources: files, dest } } else { Op::Copy { sources: files, dest } };
        let op = checked(op, expect);
        if self.start_into(op, dest_id) && was_cut {
            self.clip = None;
        }
    }

    /// Copies of the selection beside it ("name copy"). In a search the items are in their own
    /// folder, not the root's (`unavailable` makes sure there's one).
    pub fn duplicate(&mut self) {
        let sources = self.target_paths();
        let Some(dest) = sources.first().and_then(|p| p.parent()).map(Path::to_path_buf) else { return };
        if sources.iter().all(|p| p.parent() == Some(dest.as_path())) {
            self.start(checked(Op::Copy { sources, dest }, self.cur().target_checks()));
        }
    }

    pub fn trash_selected(&mut self) {
        let sources = self.target_paths();
        if !sources.is_empty() {
            self.start(checked(Op::Trash { sources }, self.cur().target_checks()));
        }
    }

    /// "Empty Trash…", confirmed: the items the dialog's count found (each deleted only while
    /// it's still that item), or what's there when the worker starts if the count hadn't
    /// landed, or found nothing it could read (the worker says why).
    pub fn empty_trash(&mut self) {
        let items = self.trash_count.take().and_then(|mut c| {
            let problem = c.got.problem.is_some();
            c.got.items.take().filter(|i| !i.is_empty() || !problem)
        });
        self.start(Op::EmptyTrash { items });
    }

    /// "Put Back": the selected items in a Trash go back where they came from.
    pub fn put_back_selected(&mut self) {
        let items = self.target_paths();
        if !items.is_empty() {
            self.start(checked(Op::PutBack { items }, self.cur().target_checks()));
        }
    }

    /// A drag out of the list ended (macOS: an OS drag session). Moved: what went is gone from
    /// here. Nothing took it: say how to move files into a folder here, since an OS drag can't
    /// land in the window it came from, and Rune doesn't say where it was let go (RUNE-GAPS.md).
    pub(crate) fn drag_out_ended(&mut self, outcome: rune::DragOutcome) {
        match outcome {
            rune::DragOutcome::Moved => self.reload(),
            rune::DragOutcome::Cancelled => {
                self.note = Some(DRAG_NOT_TAKEN.into());
            }
            _ => {}
        }
    }

    // ---- rename and new folder --------------------------------------------------------------

    /// Makes "untitled folder" here, on the worker (the window never touches the disk), and
    /// starts renaming it once it's listed: every pane showing this folder reads it again,
    /// selecting the new folder (`poll_job`).
    pub fn new_folder(&mut self) {
        let dir = self.cur().cwd.clone();
        self.start(Op::NewFolder { dir });
    }

    /// Starts an inline rename of the cursor's entry, with the name before its extension
    /// selected (as in Finder).
    pub fn start_rename(&mut self) {
        let Some(e) = self.selected() else { return };
        let (os, label, is_dir) = (e.os.clone(), e.name.clone(), e.is_dir);
        let stem = match label.rfind('.') {
            Some(i) if i > 0 && !is_dir => i,
            _ => label.len(),
        };
        let mut edit = TextEdit::new(label);
        edit.set_selection(0, stem);
        edit.focused = true;
        self.cur_mut().marked.clear();
        self.rename = Some(Rename { os, edit });
    }

    /// Return in the rename field. Renames on the worker (the window never touches the disk,
    /// which may be slow), never over an existing name. Each pane showing the folder shows the
    /// item under its new name at once, its selection following it, and reads the folder again
    /// when the rename lands (which puts it right if the rename was refused).
    pub fn commit_rename(&mut self, tx: &mut Tx) {
        // One operation at a time: the field stays open until it can go.
        if self.job.is_some() {
            self.note = Some(crate::app::BUSY.into());
            return;
        }
        let Some(r) = self.rename.take() else { return };
        tx.blur();
        let new = r.edit.text.trim().to_string();
        // The item's own name (in a search, `os` is a path below the root).
        let from = self.cur().cwd.join(&r.os);
        let Some(old) = from.file_name().map(|n| n.to_os_string()) else { return };
        if new.is_empty() || new == old.to_string_lossy() {
            return;
        }
        if new.contains('/') || (cfg!(windows) && new.contains(['\\', ':', '*', '?', '"', '<', '>', '|'])) || new == "." || new == ".." {
            self.note = Some(format!("“{new}” isn't a valid name"));
            return;
        }
        // A search result must still be the item the walk found there (checked on the worker).
        let found = self.cur().searching().then(|| self.cur().entries.iter().find(|e| e.os == r.os).and_then(|e| e.id)).flatten();
        // Beside it, in its own folder: never "the folder on screen + the new name".
        let Some(dir) = from.parent().map(Path::to_path_buf) else { return };
        let to = dir.join(&new);
        let expect: Vec<(PathBuf, crate::fs::Ident)> = found.map(|id| (from.clone(), id)).into_iter().collect();
        if !self.start(checked(Op::RenameOne { from, to }, expect)) {
            return;
        }
        // At once, before the worker answers: the panes on this folder show the new name, as a
        // label only; the row is still the item it was (and actions on it wait) until it lands.
        let f = self.cur();
        let id = if dir == f.cwd && !f.searching() { f.dir_id } else { None };
        for p in &mut self.panes {
            if !p.f.searching() && p.f.shows(&dir, id) {
                p.f.rename_pending(&old, &new);
            }
        }
    }

    // ---- undo -------------------------------------------------------------------------------

    /// Undoes the last operation on a worker, which checks the disk first. The record comes off
    /// the stack only while the undo runs: if it's refused, fails or is cancelled, the worker
    /// hands back what's left and `poll_job` puts it back (undoing isn't itself undoable).
    pub fn undo_last(&mut self) {
        if self.job.is_some() {
            self.note = Some("Still working on the last operation (Esc cancels it)".into());
            return;
        }
        let Some(u) = self.undo.pop() else {
            self.note = Some("Nothing to undo".into());
            return;
        };
        self.start(Op::Undo(u));
    }

    /// Redoes the last undone operation: the undo's own reverse, checked against the disk as
    /// the undo left it, so it refuses (and stays on the redo stack) when anything changed
    /// since. A redone operation goes back on the undo stack.
    pub fn redo_last(&mut self) {
        if self.job.is_some() {
            self.note = Some("Still working on the last operation (Esc cancels it)".into());
            return;
        }
        let Some(u) = self.redo.pop() else {
            self.note = Some("Nothing to redo".into());
            return;
        };
        self.start(Op::Undo(u));
    }

    // ---- jobs -------------------------------------------------------------------------------

    /// Starts a file operation on a worker. One at a time: false when one is still running.
    pub fn start(&mut self, op: Op) -> bool {
        self.start_into(op, None)
    }

    /// [`State::start`], for a copy or move into a folder that must still be the one with
    /// identity `dest_id` when the worker starts.
    pub fn start_into(&mut self, op: Op, dest_id: Option<crate::noclobber::FileId>) -> bool {
        if self.job.is_some() {
            self.note = Some("Still working on the last operation (Esc cancels it)".into());
            return false;
        }
        self.note = None;
        self.job_seen = (0, 0);
        // A new operation (not an undo or a redo) ends what could be redone.
        if !matches!(op, Op::Undo(_)) {
            self.redo.clear();
        }
        self.job = Some(ops::start(op, dest_id, self.waker.clone()));
        true
    }

    /// From `poll`: lands a finished job, or reports that its progress moved.
    pub fn poll_job(&mut self) -> bool {
        let Some(job) = &self.job else { return false };
        let Some(out) = job.take() else {
            let seen = (job.progress.bytes_done.load(Relaxed), job.progress.items_done.load(Relaxed));
            let moved = seen != self.job_seen;
            self.job_seen = seen;
            return moved;
        };
        let (verb, total) = (job.verb, job.progress.items_total.load(Relaxed));
        self.job = None;
        let mut out = out;
        // An undo's leftovers stay on the undo stack and what went can be redone; a redo's the
        // other way round.
        let (left, done) = if verb == "Redoing" { (&mut self.redo, &mut self.undo) } else { (&mut self.undo, &mut self.redo) };
        left.extend(out.undo.take());
        done.extend(out.inverse.take());
        // Renamed and moved items show where they are now at once (a fresh read or a recheck
        // follows); a search still walking remembers them for the batches still to come.
        if !out.moved.is_empty() || !out.removed.is_empty() {
            let renamed: Vec<(PathBuf, PathBuf)> = out.moved.iter().filter(|(a, b)| a.parent() == b.parent()).cloned().collect();
            for p in &mut self.panes {
                if p.f.searching() {
                    p.f.note_moves(&out.moved, &out.removed, &out.canon);
                } else {
                    p.f.renamed_paths(&renamed);
                }
            }
        }
        // An inline rename's label goes: the row renamed, or back to its own name.
        for p in &mut self.panes {
            p.f.land_pending(&out.moved);
        }
        // A trash and a delete have no results to select: what they did is what's gone.
        let n = if matches!(verb, "Moving to Trash" | "Deleting" | ops::EMPTYING) { out.removed.len() } else { out.results.len() };
        // Items the trash left because their disk has none: one question for all of them.
        let asks = !out.no_trash.is_empty();
        if asks {
            let why = out.no_trash_why.take().unwrap_or_default();
            self.ask_next = Some(crate::confirm::Ask::delete(std::mem::take(&mut out.no_trash), Some(&why)));
        }
        let failed = out.refused.is_some() || !out.errors.is_empty();
        let said = out.said.is_some();
        let notes = std::mem::take(&mut out.notes);
        self.note = Some(if let Some(why) = out.refused {
            why
        } else if let Some(said) = out.said.take() {
            said
        } else if verb == "Deleting" {
            deleted_note(&out, n, total, "")
        } else if verb == ops::EMPTYING {
            if !out.cancelled && out.errors.is_empty() && n == total {
                format!("Emptied the {}: deleted {n} item{} permanently", crate::trash::BIN, plural(n))
            } else {
                deleted_note(&out, n, total, &format!(" in the {}", crate::trash::BIN))
            }
        } else if !out.errors.is_empty() {
            let more = if out.errors.len() > 1 { format!(" (+{} more)", out.errors.len() - 1) } else { String::new() };
            format!("{}{more}", out.errors[0])
        } else if out.cancelled {
            format!("Cancelled after {n} item{}", plural(n))
        } else if verb == "Moving to Trash" {
            match (n, asks) {
                (0, true) => format!("Nothing was moved to the {}", crate::trash::BIN),
                (n, _) => format!("Moved {n} item{} to the {}", plural(n), crate::trash::BIN),
            }
        } else {
            let done = match verb {
                "Copying" => "Copied",
                "Moving" => "Moved",
                "Renaming" => "Renamed",
                "Redoing" => "Redid",
                "Putting Back" => "Put back",
                _ => "Restored",
            };
            format!("{done} {n} item{}", plural(n))
        });
        // What it adds (an item Put Back gave a free name).
        if let (Some(note), Some(first)) = (self.note.as_mut(), notes.first()) {
            let more = if notes.len() > 1 { format!(" (+{} more)", notes.len() - 1) } else { String::new() };
            note.push_str(&format!(". {first}{more}"));
        }
        // The inline rename and New Folder have nothing to say when they go well.
        if out.quiet && !failed && !said {
            self.note = None;
        }
        self.alert = if failed { self.note.clone() } else { None };
        self.rename = None;
        self.refresh(&out.folders, &out.results, &out.canon);
        // A new folder: renamed once it's listed in the active pane, unless the user picked
        // something else before then (`State::land`).
        if verb == ops::NEW_FOLDER
            && let Some(made) = out.results.first()
            && let Some(p) = self.cur_mut().pending.as_mut().filter(|p| Some(p.path.as_path()) == made.parent() && p.nav == Nav::Reload)
        {
            p.rename = true;
        }
        true
    }

    /// After `folders` (each with its identity, when known) changed on disk: every pane whose
    /// visible tab shows one of them reads it again (a hidden tab reads when it's shown). A pane
    /// shows a folder when it has the same identity (so a pane on a symlink to it counts), or
    /// failing that the same path. Items in `select` land selected in the pane showing their
    /// folder; elsewhere the selection stays. A pane on its way to another folder keeps going
    /// there: its read is asked again only when that folder changed too, since it may have been
    /// read before the change.
    pub(crate) fn refresh(
        &mut self,
        folders: &[(PathBuf, Option<crate::noclobber::FileId>)],
        select: &[PathBuf],
        canon: &[(PathBuf, PathBuf)],
    ) {
        for p in &mut self.panes {
            let f = &mut p.f;
            // A search re-checks the results the change could touch (through `canon`, the
            // folders' canonical paths, also when it reached them another way), and doesn't
            // walk again.
            if f.searching() {
                let changed: Vec<PathBuf> = folders.iter().filter_map(|(p, _)| f.root_based(p, canon)).collect();
                let landed: Vec<PathBuf> = select.iter().filter_map(|p| f.root_based(p, canon)).collect();
                f.recheck_results(&changed, &landed);
                continue;
            }
            let going = f.pending.as_ref().is_some_and(|p| matches!(p.nav, Nav::New | Nav::Back | Nav::Forward));
            let (at, at_id) = match &f.pending {
                // Not listed yet: its identity isn't known.
                Some(p) if going => (p.path.clone(), None),
                _ => (f.cwd.clone(), f.dir_id),
            };
            let same = |(path, id): &&(PathBuf, Option<crate::noclobber::FileId>)| match (at_id, id) {
                (Some(a), Some(b)) => a == *b,
                _ => *path == at,
            };
            let Some((changed, _)) = folders.iter().find(same) else { continue };
            let here: Vec<OsString> = select
                .iter()
                .filter(|r| r.parent() == Some(changed.as_path()))
                .filter_map(|r| r.file_name().map(|n| n.to_os_string()))
                .collect();
            match f.pending.take_if(|_| going) {
                Some(g) => f.request(g.path, g.nav, if here.is_empty() { g.select } else { here }, g.rename),
                None => crate::app::reload_folder(f, here, false),
            }
        }
    }

    /// "Copying 3 of 12 · 1.2 GB of 4 GB" for the status bar.
    pub fn job_status(&self) -> Option<String> {
        let job = self.job.as_ref()?;
        let p = &job.progress;
        let (done, total) = (p.items_done.load(Relaxed), p.items_total.load(Relaxed));
        let (bd, bt) = (p.bytes_done.load(Relaxed), p.bytes_total.load(Relaxed));
        let mut s = format!("{}… {} of {}", job.verb, (done + 1).min(total.max(1)), total.max(1));
        if bt > 0 {
            s.push_str(&format!(" · {} of {}", crate::fs::fmt_size(bd), crate::fs::fmt_size(bt)));
        }
        Some(s)
    }

    // ---- drag and drop ----------------------------------------------------------------------

    /// Files to drag out of row `i` of pane `id`: the whole selection if the row is in it,
    /// else the row. From search results, only the items still the ones the walk found (one
    /// lstat each, here: the drop happens in another app, too late to check).
    pub fn drag_item(&self, id: u64, i: usize) -> Option<DragItem> {
        let f = &self.pane(id)?.f;
        let mut paths = f.drag_paths(i);
        let stale: Vec<PathBuf> =
            f.drag_checks(i).into_iter().filter(|(p, id)| crate::fs::ident_at(p) != Some(*id)).map(|(p, _)| p).collect();
        paths.retain(|p| !stale.contains(p));
        if paths.is_empty() {
            return None;
        }
        let mut item = DragItem::new().allow_move();
        for p in paths {
            item = item.file(p);
        }
        Some(item)
    }

    /// Files dropped on a folder (or on the list, for the current folder). Same disk moves,
    /// another disk copies, as in Finder; ⌥ always copies.
    pub fn drop_files(&mut self, files: &[PathBuf], into: PathBuf, tx: &mut Tx) {
        self.drop_checked(files, into, Vec::new(), tx);
    }

    /// [`State::drop_files`] for rows dragged from search results, each checked against the
    /// identity the walk saw.
    pub(crate) fn drop_checked(&mut self, files: &[PathBuf], into: PathBuf, expect: Vec<(PathBuf, crate::fs::Ident)>, tx: &mut Tx) {
        for p in &mut self.panes {
            p.f.drop_target = None;
        }
        let files: Vec<PathBuf> = files.iter().filter(|f| f.parent() != Some(into.as_path()) || tx.mods().alt).cloned().collect();
        if files.is_empty() {
            return;
        }
        let copy = tx.mods().alt || !files.iter().all(|f| same_disk(f, &into));
        let op = if copy { Op::Copy { sources: files, dest: into } } else { Op::Move { sources: files, dest: into } };
        self.start(checked(op, expect));
    }

    // ---- context menu -----------------------------------------------------------------------

    /// Right-click on row `i` (or on empty space when `None`): a menu of commands, with their
    /// titles and keys from the command table and keymap. Native on macOS and Windows; Rune has
    /// none on Linux, so there it's Arbor's own, in the window (`menu.rs`, RUNE-GAPS.md), with
    /// the same items. On a file, macOS's menu gets an Open With submenu when the apps are
    /// read within a frame (a native menu can't change once it's open); otherwise "Open With…"
    /// opens Arbor's list.
    pub fn context_menu(&mut self, row: Option<usize>, press: PointerPress, tx: &mut Tx) {
        if let Some(i) = row
            && !self.cur().is_selected(i)
        {
            self.cur_mut().select(i);
        }
        let items = self.menu_items(row.is_some());
        let apps = if row.is_some() && self.unavailable(Cmd::OpenWith).is_none() { self.apps_for_menu() } else { None };
        self.show_menu(items, apps, press, tx);
    }

    /// Right-click on sidebar item `i` (favorites first, then volumes). Opening it reads the
    /// volumes again (one of the explicit refreshes).
    pub fn sidebar_menu(&mut self, i: usize, press: PointerPress, tx: &mut Tx) {
        // The volume as it is now: a refresh while the menu is open must not move its rows
        // onto another volume.
        self.menu_volume = i.checked_sub(self.places.len()).and_then(|v| self.volumes.get(v)).map(|v| (i, v.clone()));
        self.refresh_volumes();
        let items = self.sidebar_menu_items(i);
        self.show_menu(items, None, press, tx);
    }

    /// Right-click on the sidebar's Trash: open it, or empty it.
    pub fn trash_menu(&mut self, press: PointerPress, tx: &mut Tx) {
        let item = |cmd: Cmd, title: &str| Some((Act::Cmd(cmd), title.to_string(), self.unavailable(cmd).is_none()));
        let items = vec![item(Cmd::GoTrash, "Open"), None, item(Cmd::EmptyTrash, Cmd::EmptyTrash.title())];
        self.show_menu(items, None, press, tx);
    }

    /// Sidebar item `i`'s menu: open it, and edit the favorites or eject the volume.
    pub(crate) fn sidebar_menu_items(&self, i: usize) -> Vec<Option<(Act, String, bool)>> {
        use crate::menu::PlaceAct;
        let fav = i < self.places.len();
        let n = self.places.len();
        let item = |a: PlaceAct, title: &str, on: bool| Some((Act::Place(a, i), title.to_string(), on));
        let mut v = vec![item(PlaceAct::Open, "Open", true), item(PlaceAct::NewTab, "Open in New Tab", true), None];
        if fav {
            v.extend([
                item(PlaceAct::Remove, "Remove from Sidebar", true),
                item(PlaceAct::Up, "Move Up", i > 0),
                item(PlaceAct::Down, "Move Down", i + 1 < n),
            ]);
        } else {
            let eject = self.volumes.get(i - n).is_some_and(|v| v.eject) && self.ejecting.is_none() && self.job.is_none();
            let listed = self.place_at(i).is_some_and(|(_, p)| self.places.iter().any(|(_, q)| *q == p));
            v.extend([item(PlaceAct::Eject, "Eject", eject), item(PlaceAct::AddToSidebar, "Add to Favorites", !listed)]);
        }
        v
    }

    /// Opens `items` as the native menu, or Arbor's own where there's none. `apps`: the Open
    /// With submenu's apps (native menus only).
    fn show_menu(
        &mut self,
        items: Vec<Option<(Act, String, bool)>>,
        apps: Option<Vec<crate::openwith::Handler>>,
        press: PointerPress,
        tx: &mut Tx,
    ) {
        if !rune::popup_menu_supported() {
            self.open_menu(items, press, tx);
            return;
        }
        let mut menu = PopupMenu::new();
        for it in items {
            menu = match it {
                None => menu.separator(),
                Some((Act::Cmd(Cmd::OpenWith), _, true)) if apps.is_some() => {
                    let mut sub = PopupMenu::new();
                    for (i, h) in apps.iter().flatten().enumerate() {
                        let title = if h.default { format!("{} (default)", h.name) } else { h.name.clone() };
                        sub = sub.item(PopupItem::new(Act::OpenWith(i).id(), title));
                        if h.default && i == 0 && apps.as_ref().is_some_and(|a| a.len() > 1) {
                            sub = sub.separator();
                        }
                    }
                    menu.submenu("Open With", sub)
                }
                Some((act, title, on)) => {
                    let it = PopupItem::new(act.id(), &title).enabled(on);
                    menu.item(match act.cmd().and_then(|c| self.keymap.shortcut(c)) {
                        Some(k) => it.key_equivalent(k),
                        None => it,
                    })
                }
            };
        }
        let files: Vec<PathBuf> = {
            let f = self.cur();
            f.targets().iter().filter(|e| !e.is_dir).map(|e| f.cwd.join(&e.os)).collect()
        };
        self.menu_apps = apps.map(|a| (files, a));
        tx.popup_menu(menu, press, |s: &mut State, picked, tx| {
            tx.instant();
            if let Some(act) = picked.and_then(Act::from_id) {
                s.run_act(act, tx);
            }
            s.menu_apps = None;
        });
    }

    /// Runs a menu row's action, if it can run now.
    pub(crate) fn run_act(&mut self, act: Act, tx: &mut Tx) {
        use crate::menu::PlaceAct;
        match act {
            Act::Cmd(cmd) => {
                if self.unavailable(cmd).is_none() {
                    self.run(cmd, tx);
                }
            }
            Act::OpenWith(i) => {
                if let Some((files, apps)) = self.menu_apps.take()
                    && let Some(h) = apps.get(i).cloned()
                {
                    self.launch_with(h, files);
                }
            }
            Act::Place(a, i) => match a {
                PlaceAct::Open | PlaceAct::NewTab | PlaceAct::Eject | PlaceAct::AddToSidebar if i >= self.places.len() => {
                    // A volume's row: the volume the menu was opened on, wherever the list
                    // has moved it since.
                    let Some((_, vol)) = self.menu_volume.take().filter(|(at, _)| *at == i) else {
                        self.note = Some("The volumes changed since the menu opened; try again".into());
                        return;
                    };
                    match a {
                        PlaceAct::Eject => self.eject(vol),
                        PlaceAct::AddToSidebar => self.add_favorites(vec![vol.path], None),
                        _ => {
                            if a == PlaceAct::NewTab {
                                self.new_tab(tx);
                            }
                            if vol.path != self.cur().cwd {
                                self.go(vol.path, crate::folder::Nav::New, None);
                            }
                        }
                    }
                }
                PlaceAct::Open => self.go_place(i),
                PlaceAct::NewTab => {
                    self.new_tab(tx);
                    self.go_place(i);
                }
                PlaceAct::Remove => self.remove_favorite(i),
                PlaceAct::Up if i > 0 => self.move_favorite(i, i - 1),
                PlaceAct::Down => self.move_favorite(i, i + 1),
                PlaceAct::Up => {}
                PlaceAct::Eject => {}
                PlaceAct::AddToSidebar => {
                    if let Some((_, p)) = self.place_at(i) {
                        self.add_favorites(vec![p], None);
                    }
                }
            },
        }
        self.sync_preview();
    }

    /// The context menu's items, in order (`None` is a separator): each command, its title for
    /// what's selected, and whether it can run. `on_row`: the click was on a row.
    pub(crate) fn menu_items(&self, on_row: bool) -> Vec<Option<(Act, String, bool)>> {
        let has = on_row && !self.cur().visible.is_empty();
        let targets = if has { self.cur().targets() } else { Vec::new() };
        let n = targets.len();
        let folders = targets.iter().any(|e| e.is_dir);
        let item = |cmd: Cmd| {
            // Commands on the selection need a row under the pointer; with several, Rename is
            // Batch Rename.
            let on_selection = matches!(
                cmd,
                Cmd::Open
                    | Cmd::OpenWith
                    | Cmd::Rename
                    | Cmd::Duplicate
                    | Cmd::Trash
                    | Cmd::Copy
                    | Cmd::Cut
                    | Cmd::CopyToOther
                    | Cmd::MoveToOther
            );
            // On a row, Add to Sidebar is for the folders selected; on empty space, the folder here.
            let on = self.unavailable(cmd).is_none() && (!on_selection || has) && (cmd != Cmd::SidebarAdd || !on_row || folders);
            let title = match cmd {
                Cmd::Rename if n > 1 => format!("{} ({n} items)", Cmd::BatchRename.title()),
                Cmd::Duplicate | Cmd::Trash | Cmd::Copy | Cmd::Cut | Cmd::CopyToOther | Cmd::MoveToOther if n > 1 => {
                    format!("{} ({n} items)", cmd.title())
                }
                _ => self.title(cmd).into_owned(),
            };
            Some((Act::Cmd(cmd), title, on))
        };
        let mut v = vec![item(Cmd::Open), item(Cmd::OpenWith), item(Cmd::Reveal), None];
        // In a Trash Put Back can restore from (Linux), it comes first.
        if on_row && self.unavailable(Cmd::PutBack).is_none() {
            v.insert(0, item(Cmd::PutBack));
        }
        v.extend([item(Cmd::Info), item(Cmd::Rename), item(Cmd::Duplicate), item(Cmd::Trash), None]);
        v.extend([item(Cmd::Copy), item(Cmd::Cut), item(Cmd::Paste), item(Cmd::CopyPath), None]);
        // The commander commands only when there's another pane to send to.
        if self.split() {
            v.extend([item(Cmd::CopyToOther), item(Cmd::MoveToOther), None]);
        }
        v.extend([item(Cmd::SidebarAdd), item(Cmd::NewFolder), item(Cmd::Undo)]);
        v
    }
}

/// `op`, checked against `expect` first when there's anything to check (`Op::Checked`).
pub(crate) fn checked(op: Op, expect: Vec<(PathBuf, crate::fs::Ident)>) -> Op {
    if expect.is_empty() { op } else { Op::Checked { expect, op: Box::new(op) } }
}

/// What a permanent delete did, in words: how many items are gone (they stay gone, even after a
/// cancel), what it left and why.
/// `place` says where they were (" in the Trash"), or is empty.
fn deleted_note(out: &ops::Outcome, n: usize, total: usize, place: &str) -> String {
    let gone = match (n, total) {
        (n, t) if n == t => format!("Deleted {n} item{}{place} permanently", plural(n)),
        (n, t) => format!("Deleted {n} of {t} item{}{place} permanently", plural(t)),
    };
    let partly = out.partly.as_deref().map_or_else(String::new, |p| format!("; “{}” was partly deleted", name(p)));
    if out.cancelled {
        return format!("Cancelled: {gone}{partly}");
    }
    match out.errors.split_first() {
        None => gone,
        Some((first, rest)) => {
            let more = if rest.is_empty() { String::new() } else { format!(" (+{} more)", rest.len()) };
            format!("{gone}. {first}{more}")
        }
    }
}

/// What a drag out of the list that nothing took says.
pub(crate) const DRAG_NOT_TAKEN: &str =
    "Nothing took the drop. To move files into a folder in this window, cut and paste them (⌘X, ⌘V) or use split view (⌘\\).";

fn name(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(unix)]
fn same_disk(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (a.symlink_metadata(), b.metadata()) {
        (Ok(x), Ok(y)) => x.dev() == y.dev(),
        _ => false,
    }
}

#[cfg(windows)]
fn same_disk(a: &Path, b: &Path) -> bool {
    a.components().next() == b.components().next()
}
