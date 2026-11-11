//! File operations from the UI: clipboard, paste, duplicate, rename, new folder and file, links, trash, undo,
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
        *self.shared.clip.borrow_mut() = Some(Clip { paths, cut, expect });
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
            files = self.shared.clip.borrow().as_ref().map(|c| c.paths.clone()).unwrap_or_default();
        }
        if files.is_empty() {
            self.note = Some("Nothing to paste: copy some files first".into());
            return;
        }
        let (was_cut, expect) = {
            let clip = self.shared.clip.borrow();
            let ours = clip.as_ref().filter(|c| c.paths == files);
            (ours.is_some_and(|c| c.cut), ours.map(|c| c.expect.clone()).unwrap_or_default())
        };
        let op = if move_requested || was_cut { Op::Move { sources: files, dest } } else { Op::Copy { sources: files, dest } };
        let op = checked(op, expect);
        if self.start_into(op, dest_id.map(|id| (ops::DestId::Folder(id), "pasted"))) && was_cut {
            *self.shared.clip.borrow_mut() = None;
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

    /// Compress: the selection into one `.zip` beside it ("Photos.zip", or "Archive.zip" for
    /// several). In a search the items are in their own folder (`unavailable` makes sure there's
    /// one).
    pub fn compress_selected(&mut self) {
        let sources = self.target_paths();
        let Some(dir) = sources.first().and_then(|p| p.parent()).map(Path::to_path_buf) else { return };
        if sources.iter().all(|p| p.parent() == Some(dir.as_path())) {
            self.start(checked(Op::Compress { sources, dir }, self.cur().target_checks()));
        }
    }

    /// Extract Here: each selected archive into a folder beside it.
    pub fn extract_selected(&mut self) {
        let archives = self.target_paths();
        if !archives.is_empty() {
            self.start(checked(Op::Extract { archives }, self.cur().target_checks()));
        }
    }

    pub fn trash_selected(&mut self) {
        let sources = self.target_paths();
        if !sources.is_empty() {
            self.start(checked(Op::Trash { sources }, self.cur().target_checks()));
        }
    }

    /// "Empty Trash…", confirmed: the items the dialog's count found, each deleted only while
    /// it's still that item (on Windows the shell empties the Recycle Bin it counted). Never
    /// without a count: the dialog offers no button before one lands, and nothing here lists
    /// the Trash afresh.
    pub fn empty_trash(&mut self) {
        let Some(mut c) = self.trash_count.take().filter(|c| c.got.count.is_some()) else {
            self.note = Some(format!("Empty the {} once it's counted", crate::trash::BIN));
            return;
        };
        let items = c.got.items.take();
        if items.as_ref().is_some_and(Vec::is_empty) || c.got.count == Some(0) {
            self.note = Some(format!("Nothing in the {} to empty", crate::trash::BIN));
            return;
        }
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

    // ---- rename, new folder, new file, links -----------------------------------------------

    /// Makes "untitled folder" here, on the worker (the window never touches the disk), and
    /// starts renaming it once it's listed: every pane showing this folder reads it again,
    /// selecting the new folder (`poll_job`). Only into the folder as it was listed (`here`).
    pub fn new_folder(&mut self) {
        let (dir, dest) = self.here("made");
        self.start_into(Op::NewFolder { dir }, dest);
    }

    /// New File: an empty "untitled.txt" here (the active column's folder in the column view),
    /// made and renamed as New Folder's folder is.
    pub fn new_file(&mut self) {
        let (dir, dest) = self.here("made");
        self.start_into(Op::NewFile { dir }, dest);
    }

    /// The active folder, and its identity as it was listed for the worker to check before
    /// making anything in it: a folder put at its path since is refused ("Nothing was made").
    /// One whose identity isn't known yet (still being read) is used as it is, as before.
    fn here(&self, verb: &'static str) -> (PathBuf, Option<(ops::DestId, &'static str)>) {
        let f = self.cur();
        (f.cwd.clone(), f.dir_id.map(|id| (ops::DestId::Folder(id), verb)))
    }

    /// Make Symbolic Link: a link beside each selected item, each checked on the worker to be
    /// the item its listing (or the search) found. They're selected once listed.
    pub fn make_links(&mut self) {
        let items = self.target_paths();
        if !items.is_empty() {
            self.start(checked(Op::MakeLinks { items }, self.cur().target_checks()));
        }
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

    /// Esc: the inline rename ends with nothing renamed.
    pub fn cancel_rename(&mut self, tx: &mut Tx) {
        self.rename = None;
        tx.blur();
    }

    /// A key in the focused rename field, before the field's editor sees it. Esc cancels in one
    /// press: left to the editor, Rune's field only blurs itself on Esc and keeps the key, which
    /// left the field on screen until a second Esc.
    pub fn rename_field_key(&mut self, ev: &rune::KeyEvent, tx: &mut Tx) -> bool {
        if ev.key != rune::Key::Escape {
            return false;
        }
        tx.instant();
        self.cancel_rename(tx);
        true
    }

    /// Return in the rename field. Renames on the worker (the window never touches the disk,
    /// which may be slow), never over an existing name. Each pane showing the folder shows the
    /// item under its new name at once, its selection following it, and reads the folder again
    /// when the rename lands (which puts it right if the rename was refused).
    pub fn commit_rename(&mut self, tx: &mut Tx) {
        // One operation at a time: the field stays open until it can go.
        if self.busy() {
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
        // It must still be the item its listing (or the search walk) found there, edited or not
        // (checked on the worker).
        let found = self.cur().entries.iter().find(|e| e.os == r.os).map(|e| e.id);
        let chosen = self.cur().chosen();
        // Beside it, in its own folder: never "the folder on screen + the new name".
        let Some(dir) = from.parent().map(Path::to_path_buf) else { return };
        let to = dir.join(&new);
        let expect = ops::Checks::new(chosen, found.map(|id| (from.clone(), id)).into_iter().collect());
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
        if self.busy() {
            self.note = Some(self.busy_note().into());
            return;
        }
        let Some((n, u)) = self.shared.undo.borrow_mut().pop() else {
            self.note = Some("Nothing to undo".into());
            return;
        };
        self.start_job(Op::Undo(u), None, Some(n));
    }

    /// Redoes the last undone operation: the undo's own reverse, checked against the disk as
    /// the undo left it, so it refuses (and stays on the redo stack) when anything changed
    /// since. A redone operation goes back on the undo stack.
    pub fn redo_last(&mut self) {
        if self.busy() {
            self.note = Some(self.busy_note().into());
            return;
        }
        let Some((n, u)) = self.shared.redo.borrow_mut().pop() else {
            self.note = Some("Nothing to redo".into());
            return;
        };
        self.start_job(Op::Undo(u), None, Some(n));
    }

    // ---- jobs -------------------------------------------------------------------------------

    /// Starts a file operation on a worker. One at a time: false when one is still running.
    pub fn start(&mut self, op: Op) -> bool {
        self.start_into(op, None)
    }

    /// [`State::start`], for a copy or move into a folder that must still be the one with
    /// identity `dest_id` when the worker starts (with the verb its refusal says).
    pub fn start_into(&mut self, op: Op, dest_id: Option<(ops::DestId, &'static str)>) -> bool {
        self.start_job(op, dest_id, None)
    }

    /// [`State::start_into`]; `undoing`: the outcome an undo or redo takes back.
    fn start_job(&mut self, op: Op, dest_id: Option<(ops::DestId, &'static str)>, undoing: Option<u64>) -> bool {
        if self.busy() {
            self.note = Some(self.busy_note().into());
            return false;
        }
        self.forget_leftovers();
        self.note = None;
        // A new operation (not an undo or a redo) ends what could be redone.
        if !matches!(op, Op::Undo(_)) {
            self.shared.redo.borrow_mut().clear();
        }
        // The worker wakes whichever window owns the operation: this one, or the one made its
        // owner if this one closes first (`State::leave`).
        let waker = self.waker.as_ref().map(|_| self.shared.op_wake());
        self.set_job(ops::start(op, dest_id, waker));
        if let Some(j) = self.shared.job.borrow_mut().as_mut() {
            j.undoing = undoing;
        }
        true
    }

    /// `run` is the app's file operation, this window's to show.
    pub(crate) fn set_job(&mut self, run: ops::Running) {
        if let Ok(mut w) = self.shared.op_waker.lock() {
            w.clone_from(&self.waker);
        }
        *self.shared.job.borrow_mut() = Some(crate::app::Job { run, owner: self.win, seen: (0, 0), undoing: None, asked: self.win });
    }

    /// Outcome `n` was undone (or redone) from some window: its note, if this window still
    /// says it, goes.
    pub(crate) fn outcome_undone(&mut self, n: u64) {
        if let Some((of, text)) = &self.note_of
            && *of == n
            && self.note.as_deref() == Some(text.as_str())
        {
            self.note = None;
            self.alert = None;
        }
        if self.note_of.as_ref().is_some_and(|(of, _)| *of == n) {
            self.note_of = None;
        }
    }

    /// Why a command that starts a file operation can't run: one is running, here or in
    /// another window.
    pub(crate) fn busy_why(&self) -> &'static str {
        if self.owns_job() { crate::app::BUSY } else { crate::app::BUSY_ELSEWHERE }
    }

    /// Why a new operation can't start while one runs: Esc cancels it here; in another window,
    /// it's there to cancel.
    pub(crate) fn busy_note(&self) -> &'static str {
        if self.owns_job() { "Still working on the last operation (Esc cancels it)" } else { crate::app::BUSY_ELSEWHERE }
    }

    /// From `poll`: lands the app's finished file operation, or reports that its progress moved,
    /// in the window that owns it, or in any window when its owner has gone (which then owns
    /// it). The outcome lands once: whichever window takes it from `Shared` applies it to the
    /// shared undo stack and tells the others what changed.
    pub fn poll_job(&mut self) -> bool {
        let (out, verb, total, undoing, asked) = {
            let mut slot = self.shared.job.borrow_mut();
            let Some(job) = slot.as_mut() else { return false };
            if job.owner != self.win && self.shared.has(job.owner) {
                return false;
            }
            let adopt = job.owner != self.win;
            job.owner = self.win;
            let Some(out) = job.run.take() else {
                let p = &job.run.progress;
                let seen = (p.bytes_done.load(Relaxed), p.items_done.load(Relaxed));
                let moved = seen != job.seen || adopt;
                job.seen = seen;
                drop(slot);
                if adopt && let Ok(mut w) = self.shared.op_waker.lock() {
                    w.clone_from(&self.waker);
                }
                return moved;
            };
            let job = slot.take().expect("just seen");
            (out, job.run.verb, job.run.progress.items_total.load(Relaxed), job.undoing, job.asked)
        };
        let outcome = self.shared.outcomes.get() + 1;
        self.shared.outcomes.set(outcome);
        let mut out = out;
        // An undo's leftovers stay on the undo stack (still the outcome they undo) and what
        // went can be redone (this outcome); a redo's the other way round. One pair of stacks for
        // every window.
        {
            let (mut undo, mut redo) = (self.shared.undo.borrow_mut(), self.shared.redo.borrow_mut());
            let (left, done) = if verb == "Redoing" { (&mut *redo, &mut *undo) } else { (&mut *undo, &mut *redo) };
            left.extend(out.undo.take().map(|u| (undoing.unwrap_or(outcome), u)));
            done.extend(out.inverse.take().map(|u| (outcome, u)));
        }
        // An undo or redo takes back an earlier outcome: no window goes on saying it happened.
        if let Some(was) = undoing {
            self.shared.tell_others(self.win, || Box::new(move |s, _| s.outcome_undone(was)));
        }
        // What changed on disk, for every window: this one's panes now, the others' through
        // their inboxes (each reads the folders it shows that changed, keeping its selection).
        let fx = std::rc::Rc::new(Changed {
            moved: std::mem::take(&mut out.moved),
            removed: std::mem::take(&mut out.removed),
            canon: std::mem::take(&mut out.canon),
            reidentified: std::mem::take(&mut out.reidentified),
            folders: std::mem::take(&mut out.folders),
        });
        self.shared.tell_others(self.win, || {
            let fx = fx.clone();
            Box::new(move |s, _| s.changed_on_disk(&fx, &[]))
        });
        // A trash and a delete have no results to select: what they did is what's gone.
        let n = if matches!(verb, "Moving to Trash" | "Deleting" | ops::EMPTYING) { fx.removed.len() } else { out.results.len() };
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
        } else if let Some((what, redo)) = out.undid {
            ops::undone_note(what, redo, &out.results)
        } else if verb == ops::MAKING_LINKS {
            format!("Made {n} link{}", plural(n))
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
        // The inline rename, New Folder and New File have nothing to say when they go well.
        if out.quiet && !failed && !said {
            self.note = None;
        }
        self.alert = if failed { self.note.clone() } else { None };
        // This outcome's note, so an undo of it from any window takes it away.
        self.note_of = self.note.clone().map(|t| (outcome, t));
        self.rename = None;
        self.changed_on_disk(&fx, &out.results);
        // A new folder or file: renamed once it's listed in the active pane, unless the user
        // picked something else before then (`State::land`), and only that item
        // (`State::rename_made`). A filter that would hide it is cleared, so it shows. Only in
        // the window that asked for it (another may have landed it, its owner having closed).
        self.made = None;
        if (verb == ops::NEW_FOLDER || verb == ops::NEW_FILE)
            && asked == self.win
            && let Some(made) = out.results.first().cloned()
            && let Some(p) = self.cur_mut().pending.as_mut().filter(|p| Some(p.path.as_path()) == made.parent() && p.nav == Nav::Reload)
        {
            p.rename = true;
            let f = self.cur_mut();
            if !f.filter.is_empty() {
                f.filter.clear();
                f.goto = false;
            }
            self.made = Some((made, out.made));
        }
        true
    }

    /// What a file operation changed (`fx`), in this window: renamed and moved items show where
    /// they are now at once (a fresh read or a recheck follows; a search still walking
    /// remembers them for the batches still to come), an inline rename's label goes, rows whose
    /// identity Arbor's action changed take the new one, and every pane on a changed folder
    /// reads it again, `select`ing what the operation made where it lands.
    pub(crate) fn changed_on_disk(&mut self, fx: &Changed, select: &[PathBuf]) {
        if !fx.moved.is_empty() || !fx.removed.is_empty() {
            let renamed: Vec<(PathBuf, PathBuf)> = fx.moved.iter().filter(|(a, b)| a.parent() == b.parent()).cloned().collect();
            for p in &mut self.panes {
                if p.f.searching() {
                    p.f.note_moves(&fx.moved, &fx.removed, &fx.canon);
                } else {
                    p.f.renamed_paths(&renamed);
                }
            }
        }
        // An inline rename's label goes: the row renamed, or back to its own name. Rows of items
        // Arbor's action changed the ctime of take what it made of them, where that's identity.
        for p in &mut self.panes {
            p.f.land_pending(&fx.moved);
            p.f.reidentify(&fx.reidentified);
        }
        self.refresh(&fx.folders, select, &fx.canon);
        // The space an operation freed or took: each pane's volume read again, once (a pane
        // reading its folder again asked already; a newer request replaces an older one).
        for p in &mut self.panes {
            if p.f.pending.is_none() && !p.f.searching() {
                p.f.ask_free();
            }
        }
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
        folders: &[(PathBuf, Option<crate::noclobber::DirId>)],
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
            // The column view's other columns showing a changed folder read it again too.
            if let Some(cols) = f.cols.as_mut() {
                cols.reread(folders, false);
            }
            let going = f.pending.as_ref().is_some_and(|p| matches!(p.nav, Nav::New | Nav::Back | Nav::Forward));
            let (at, at_id) = match &f.pending {
                // Not listed yet: its identity isn't known.
                Some(p) if going => (p.path.clone(), None),
                _ => (f.cwd.clone(), f.dir_id),
            };
            let same = |(path, id): &&(PathBuf, Option<crate::noclobber::DirId>)| match (at_id, id) {
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
        let slot = self.shared.job.borrow();
        let job = &slot.as_ref().filter(|j| j.owner == self.win)?.run;
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
    /// else the row, as listed. Nothing is read from the disk here, on the window's thread (a
    /// slow disk would freeze it): a drop inside the window goes to the worker, which checks each
    /// item against its listing's identity right before acting (`State::drop_on_pane`); what
    /// another app does with a drop is that app's to check.
    pub fn drag_item(&self, id: u64, i: usize) -> Option<DragItem> {
        let paths = self.pane(id)?.f.drag_paths(i);
        if paths.is_empty() {
            return None;
        }
        let mut item = DragItem::new().allow_move();
        for p in paths {
            item = item.file(p);
        }
        Some(item)
    }

    /// Files dropped on a folder: every drop, from other apps or Arbor's own rows (each of
    /// those checked against the identity its listing or the walk saw, `expect`), on any view's
    /// folder row or background, a column, or the other pane. Same disk moves, another disk
    /// copies, as in Finder; ⌥ always copies. The folder goes with the identity it was listed
    /// with, and the worker refuses one put in its place since, as a paste does; one whose
    /// identity wasn't known (not read yet, or unreadable) takes nothing.
    pub(crate) fn drop_into(&mut self, files: &[PathBuf], dest: ops::Dest, expect: ops::Checks, tx: &mut Tx) {
        for p in &mut self.panes {
            p.f.drop_target = None;
            if let Some(cols) = p.f.cols.as_mut() {
                cols.drop_target = None;
            }
        }
        let alt = tx.mods().alt;
        let files: Vec<PathBuf> = files.iter().filter(|f| f.parent() != Some(dest.path.as_path()) || alt).cloned().collect();
        if files.is_empty() {
            return;
        }
        let copy = alt || !files.iter().all(|f| same_disk(f, &dest.path));
        self.send_into(files, dest, expect, !copy, "dropped");
    }

    /// What every copy or move into a folder the user chose on screen goes through (a drop,
    /// F5 and F6 into the other pane): into `dest` as it was listed, the worker refusing a
    /// folder put in its place since, and one whose identity wasn't known taking nothing, each
    /// item checked against `expect`. `verb` is what its refusal says wasn't done.
    pub(crate) fn send_into(&mut self, files: Vec<PathBuf>, dest: ops::Dest, expect: ops::Checks, mv: bool, verb: &'static str) {
        let ops::Dest { path: into, id } = dest;
        let Some(id) = id else {
            let name = into.file_name().map_or_else(|| into.display().to_string(), |n| n.to_string_lossy().into_owned());
            self.note = Some(format!("Nothing was {verb}: “{name}” isn't read yet, so Arbor can't tell it's the folder shown"));
            return;
        };
        let op = if mv { Op::Move { sources: files, dest: into } } else { Op::Copy { sources: files, dest: into } };
        self.start_into(checked(op, expect), Some((id, verb)));
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

    /// Right-click on the list's header row: each column, checked while shown (Name always
    /// is), Folders First, and Reset Columns.
    pub fn header_menu(&mut self, press: PointerPress, tx: &mut Tx) {
        let items = self.header_menu_items();
        self.show_menu(items, None, press, tx);
    }

    pub(crate) fn header_menu_items(&self) -> Vec<Option<(Act, String, bool)>> {
        let mut v: Vec<Option<(Act, String, bool)>> =
            crate::listcols::Field::ALL.into_iter().map(|f| Some((Act::Cmd(f.cmd()), f.title().to_string(), true))).collect();
        v.push(None);
        v.push(Some((Act::Cmd(Cmd::FoldersFirst), "Folders First".into(), true)));
        v.push(None);
        v.push(Some((Act::Cmd(Cmd::ResetColumns), Cmd::ResetColumns.title().into(), self.unavailable(Cmd::ResetColumns).is_none())));
        v
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
        let mut v = vec![
            item(PlaceAct::Open, "Open", true),
            item(PlaceAct::NewTab, "Open in New Tab", true),
            item(PlaceAct::NewWindow, Cmd::OpenInNewWindow.title(), true),
            None,
        ];
        if fav {
            v.extend([
                item(PlaceAct::Remove, "Remove from Sidebar", true),
                item(PlaceAct::Up, "Move Up", i > 0),
                item(PlaceAct::Down, "Move Down", i + 1 < n),
            ]);
        } else {
            let eject = self.volumes.get(i - n).is_some_and(|v| v.eject) && self.ejecting.is_none() && !self.busy();
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
                    let checked = act.cmd().and_then(|c| self.checked(c)) == Some(true);
                    let it = PopupItem::new(act.id(), &title).enabled(on).checked(checked);
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
                PlaceAct::Open | PlaceAct::NewTab | PlaceAct::NewWindow | PlaceAct::Eject | PlaceAct::AddToSidebar
                    if i >= self.places.len() =>
                {
                    // A volume's row: the volume the menu was opened on, wherever the list
                    // has moved it since.
                    let Some((_, vol)) = self.menu_volume.take().filter(|(at, _)| *at == i) else {
                        self.note = Some("The volumes changed since the menu opened; try again".into());
                        return;
                    };
                    match a {
                        PlaceAct::Eject => self.eject(vol),
                        PlaceAct::AddToSidebar => self.add_favorites(vec![vol.path], None),
                        PlaceAct::NewWindow => self.open_window(vol.path),
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
                // A favorite that turns out to be gone shows the read's error in the new window.
                PlaceAct::NewWindow => {
                    if let Some((_, p)) = self.place_at(i) {
                        self.open_window(p);
                    }
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
                    | Cmd::OpenInNewWindow
                    | Cmd::OpenWith
                    | Cmd::Rename
                    | Cmd::Duplicate
                    | Cmd::MakeLink
                    | Cmd::Compress
                    | Cmd::Extract
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
                Cmd::Duplicate | Cmd::MakeLink | Cmd::Trash | Cmd::Copy | Cmd::Cut | Cmd::CopyToOther | Cmd::MoveToOther if n > 1 => {
                    format!("{} ({n} items)", cmd.title())
                }
                _ => self.title(cmd).into_owned(),
            };
            Some((Act::Cmd(cmd), title, on))
        };
        let mut v = vec![item(Cmd::Open), item(Cmd::OpenWith), item(Cmd::Reveal)];
        // A folder can open in a window of its own.
        if on_row && folders && n == 1 {
            v.insert(1, item(Cmd::OpenInNewWindow));
        }
        // On a symbolic link, its original.
        if has && self.unavailable(Cmd::ShowOriginal).is_none() {
            v.push(item(Cmd::ShowOriginal));
        }
        v.push(None);
        // In a Trash Put Back can restore from (Linux), it comes first.
        if on_row && self.unavailable(Cmd::PutBack).is_none() {
            v.insert(0, item(Cmd::PutBack));
        }
        v.extend([item(Cmd::Info), item(Cmd::Rename), item(Cmd::Duplicate), item(Cmd::MakeLink), item(Cmd::Compress)]);
        // Extract Here only where the selection is archives.
        if has && self.unavailable(Cmd::Extract).is_none() {
            v.push(item(Cmd::Extract));
        }
        v.extend([item(Cmd::Trash), None]);
        v.extend([item(Cmd::Copy), item(Cmd::Cut), item(Cmd::Paste), item(Cmd::CopyPath), None]);
        // The commander commands only when there's another pane to send to.
        if self.split() {
            v.extend([item(Cmd::CopyToOther), item(Cmd::MoveToOther), None]);
        }
        v.extend([item(Cmd::SidebarAdd), item(Cmd::NewFolder), item(Cmd::NewFile), item(Cmd::Undo)]);
        v
    }
}

/// What a file operation changed on disk, as every window needs it (`State::changed_on_disk`).
pub(crate) struct Changed {
    pub moved: Vec<(PathBuf, PathBuf)>,
    pub removed: Vec<PathBuf>,
    pub canon: Vec<(PathBuf, PathBuf)>,
    pub reidentified: Vec<(crate::fs::Ident, crate::fs::Ident)>,
    pub folders: Vec<(PathBuf, Option<crate::noclobber::DirId>)>,
}

/// `op`, checked against `expect` first when there's anything to check (`Op::Checked`).
pub(crate) fn checked(op: Op, expect: ops::Checks) -> Op {
    if expect.items.is_empty() { op } else { Op::Checked { expect, op: Box::new(op) } }
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

/// The mouse buttons down now, as the window server says (`NSEvent.pressedMouseButtons`: bit 0
/// is the left button), on macOS's main thread; `None` elsewhere (no system drags there).
pub(crate) fn pressed_buttons() -> Option<usize> {
    #[cfg(test)]
    if let Some(mask) = PRETEND_PRESSED.with(std::cell::Cell::get) {
        return Some(mask);
    }
    #[cfg(target_os = "macos")]
    {
        use objc2::{class, msg_send};
        // SAFETY: class methods with no arguments, on the main thread only.
        unsafe {
            let main: bool = msg_send![class!(NSThread), isMainThread];
            if !main {
                return None;
            }
            let mask: usize = msg_send![class!(NSEvent), pressedMouseButtons];
            Some(mask)
        }
    }
    #[cfg(not(target_os = "macos"))]
    None
}

/// Whether a system drag session that has just started was left waiting for a release that
/// already happened: the left button isn't down any more (`pressed`, [`pressed_buttons`]).
pub(crate) fn drag_left_behind(pressed: Option<usize>) -> bool {
    pressed.is_some_and(|mask| mask & 1 == 0)
}

/// Ends a system drag session waiting for a release that already happened, by giving the app
/// that release again: a left-mouse-up at the pointer, posted to the front of Arbor's own event
/// queue (`-[NSApplication postEvent:atStart:]`; in-process, so no permission and nothing
/// outside the app). The session's tracking loop takes it as the drop, over Arbor's own window,
/// which takes no drops from its own drags, so the session ends Cancelled and the status bar
/// says how to move files. True when it posted.
pub(crate) fn release_left_behind_drag() -> bool {
    #[cfg(test)]
    if PRETEND_PRESSED.with(std::cell::Cell::get).is_some() {
        RELEASES_POSTED.with(|n| n.set(n.get() + 1));
        return true;
    }
    #[cfg(target_os = "macos")]
    {
        use objc2::runtime::AnyObject;
        use objc2::{class, msg_send};
        use objc2_core_foundation::CGPoint;
        // SAFETY: AppKit calls on the main thread with live objects: the shared application,
        // its key (else main) window, and an event from AppKit's own constructor.
        unsafe {
            let main: bool = msg_send![class!(NSThread), isMainThread];
            if !main {
                return false;
            }
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            if app.is_null() {
                return false;
            }
            let mut window: *mut AnyObject = msg_send![app, keyWindow];
            if window.is_null() {
                window = msg_send![app, mainWindow];
            }
            if window.is_null() {
                return false;
            }
            let at: CGPoint = msg_send![window, mouseLocationOutsideOfEventStream];
            let number: isize = msg_send![window, windowNumber];
            let ev = left_mouse_up(at, number);
            if ev.is_null() {
                return false;
            }
            let _: () = msg_send![app, postEvent: ev, atStart: true];
            true
        }
    }
    #[cfg(not(target_os = "macos"))]
    false
}

/// A left-mouse-up event at `at` (window points) in window `number`, now. Made, not posted.
#[cfg(target_os = "macos")]
fn left_mouse_up(at: objc2_core_foundation::CGPoint, number: isize) -> *mut objc2::runtime::AnyObject {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    // NSEventTypeLeftMouseUp.
    const LEFT_MOUSE_UP: usize = 2;
    // SAFETY: AppKit's documented constructor, with arguments of the types it declares.
    unsafe {
        let info: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
        let now: f64 = msg_send![info, systemUptime];
        msg_send![class!(NSEvent),
            mouseEventWithType: LEFT_MOUSE_UP,
            location: at,
            modifierFlags: 0usize,
            timestamp: now,
            windowNumber: number,
            context: std::ptr::null_mut::<AnyObject>(),
            eventNumber: 0isize,
            clickCount: 1isize,
            pressure: 0.0f32]
    }
}

/// macOS: the release Arbor would post is a left-mouse-up where asked (made, never posted:
/// this checks the AppKit calls' signatures, which `msg_send!` verifies only as they run).
#[cfg(all(test, target_os = "macos"))]
#[test]
fn the_release_given_again_is_a_left_mouse_up() {
    use objc2::msg_send;
    let ev = left_mouse_up(objc2_core_foundation::CGPoint::new(12.0, 34.0), 0);
    assert!(!ev.is_null());
    // SAFETY: a live event AppKit just made.
    let (kind, at, clicks): (usize, objc2_core_foundation::CGPoint, isize) =
        unsafe { (msg_send![ev, type], msg_send![ev, locationInWindow], msg_send![ev, clickCount]) };
    assert_eq!((kind, at.x, at.y, clicks), (2, 12.0, 34.0, 1));
}

#[cfg(test)]
thread_local! {
    /// Tests: the mouse buttons [`pressed_buttons`] pretends are down, and how many releases
    /// [`release_left_behind_drag`] would have posted.
    pub(crate) static PRETEND_PRESSED: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    pub(crate) static RELEASES_POSTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl State {
    /// A row's system drag session started (macOS; Rune says so in the frame after it began
    /// it). If the button came up before the session took over the mouse, that release went to
    /// the window as an ordinary mouse-up, and the session would wait for another, eating every
    /// key until Esc (RUNE-GAPS.md): it's given the release again.
    pub(crate) fn drag_out_started(&mut self) {
        let pressed = pressed_buttons();
        let posted = drag_left_behind(pressed) && release_left_behind_drag();
        crate::draglog::log(|| {
            let mask = pressed.map_or("-".into(), |m| m.to_string());
            format!("drag-out started pressed={mask}{}", if posted { " -> released (posted a left-mouse-up)" } else { "" })
        });
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
