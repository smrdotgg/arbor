//! File operations from the UI: clipboard, paste, duplicate, rename, new folder, trash, undo,
//! drag and drop, and the context menu. The work itself runs in `ops` on a worker thread.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;

use rune::{ClipboardContent, DragItem, PointerPress, PopupItem, PopupMenu, TextEdit, Tx};

use crate::app::{Clip, Rename, State};
use crate::ops::{self, Op, Undo};

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
        self.clip = Some(Clip { paths, cut });
    }

    /// ⌘⇧C: the selected path as text.
    pub fn copy_path(&mut self, tx: &mut Tx) {
        let paths = self.target_paths();
        let text = if paths.is_empty() {
            self.cwd.display().to_string()
        } else {
            paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n")
        };
        self.note = Some(format!("Copied {text}"));
        tx.copy(text);
    }

    /// The root's paste handler: files from the clipboard (or absolute paths in its text) go
    /// into the current folder. A paste of what Arbor itself cut, or ⌥⌘V, moves.
    pub fn paste(&mut self, content: &ClipboardContent) {
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
        let move_requested = std::mem::take(&mut self.paste_move);
        if files.is_empty() {
            self.note = Some("Nothing to paste: copy some files first".into());
            return;
        }
        let was_cut = self.clip.as_ref().is_some_and(|c| c.cut && c.paths == files);
        let dest = self.cwd.clone();
        let op = if move_requested || was_cut { Op::Move { sources: files, dest } } else { Op::Copy { sources: files, dest } };
        if self.start(op) && was_cut {
            self.clip = None;
        }
    }

    pub fn duplicate(&mut self) {
        let sources = self.target_paths();
        if !sources.is_empty() {
            self.start(Op::Copy { sources, dest: self.cwd.clone() });
        }
    }

    pub fn trash_selected(&mut self) {
        let sources = self.target_paths();
        if !sources.is_empty() {
            self.start(Op::Trash { sources });
        }
    }

    // ---- rename and new folder --------------------------------------------------------------

    pub fn new_folder(&mut self) {
        let path = ops::free_name(&self.cwd, "untitled folder".as_ref(), false);
        match std::fs::create_dir(&path) {
            Ok(()) => {
                self.undo.extend(Undo::new_folder(path.clone()));
                let name = path.file_name().map(|n| n.to_os_string()).into_iter().collect();
                self.reload_selecting(name, true);
            }
            Err(e) => self.note = Some(format!("Couldn't make a folder: {e}")),
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
        self.marked.clear();
        self.rename = Some(Rename { os, edit });
    }

    /// Return in the rename field. Renames on the spot (it's one syscall), never over an
    /// existing name.
    pub fn commit_rename(&mut self, tx: &mut Tx) {
        let Some(r) = self.rename.take() else { return };
        tx.blur();
        let new = r.edit.text.trim().to_string();
        let old_name = r.os.to_string_lossy();
        if new.is_empty() || new == old_name {
            return;
        }
        if new.contains('/') || (cfg!(windows) && new.contains(['\\', ':', '*', '?', '"', '<', '>', '|'])) || new == "." || new == ".." {
            self.note = Some(format!("“{new}” isn't a valid name"));
            return;
        }
        let from = self.cwd.join(&r.os);
        let to = self.cwd.join(&new);
        // A case-only change is the same file on case-insensitive disks; let it through.
        let case_only = new.to_lowercase() == old_name.to_lowercase();
        if !case_only && to.symlink_metadata().is_ok() {
            self.note = Some(format!("“{new}” already exists"));
            return;
        }
        match std::fs::rename(&from, &to) {
            Ok(()) => {
                // One lstat to fingerprint it, so undo can tell whether it changed since.
                self.undo.extend(Undo::renamed(to, from));
                self.note = None;
                self.reload_selecting(vec![OsString::from(new)], false);
            }
            Err(e) => self.note = Some(format!("Couldn't rename: {e}")),
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

    // ---- jobs -------------------------------------------------------------------------------

    /// Starts a file operation on a worker. One at a time: false when one is still running.
    pub fn start(&mut self, op: Op) -> bool {
        if self.job.is_some() {
            self.note = Some("Still working on the last operation (Esc cancels it)".into());
            return false;
        }
        self.note = None;
        self.job_seen = (0, 0);
        self.job = Some(ops::start(op, self.waker.clone()));
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
        let verb = job.verb;
        self.job = None;
        self.undo.extend(out.undo);
        let n = out.results.len();
        self.note = Some(if let Some(why) = out.refused {
            why
        } else if !out.errors.is_empty() {
            let more = if out.errors.len() > 1 { format!(" (+{} more)", out.errors.len() - 1) } else { String::new() };
            format!("{}{more}", out.errors[0])
        } else if out.cancelled {
            format!("Cancelled after {n} item{}", plural(n))
        } else {
            let done = match verb {
                "Copying" => "Copied",
                "Moving" => "Moved",
                "Moving to Trash" => "Moved to Trash",
                _ => "Restored",
            };
            format!("{done} {n} item{}", plural(n))
        });
        // Select what landed here; otherwise keep the selection.
        let here: Vec<OsString> = out
            .results
            .iter()
            .filter(|p| p.parent() == Some(self.cwd.as_path()))
            .filter_map(|p| p.file_name().map(|n| n.to_os_string()))
            .collect();
        self.reload_selecting(here, false);
        true
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

    /// Files to drag out of row `i`: the whole selection if the row is in it, else the row.
    pub fn drag_item(&self, i: usize) -> Option<DragItem> {
        let paths = if self.is_selected(i) {
            self.target_paths()
        } else {
            let e = &self.entries[*self.visible.get(i)? as usize];
            vec![self.cwd.join(&e.os)]
        };
        let mut item = DragItem::new().allow_move();
        for p in paths {
            item = item.file(p);
        }
        Some(item)
    }

    /// Files dropped on a folder (or on the list, for the current folder). Same disk moves,
    /// another disk copies, as in Finder; ⌥ always copies.
    pub fn drop_files(&mut self, files: &[PathBuf], into: PathBuf, tx: &mut Tx) {
        self.drop_target = None;
        let files: Vec<PathBuf> = files.iter().filter(|f| f.parent() != Some(into.as_path()) || tx.mods().alt).cloned().collect();
        if files.is_empty() {
            return;
        }
        let copy = tx.mods().alt || !files.iter().all(|f| same_disk(f, &into));
        self.start(if copy { Op::Copy { sources: files, dest: into } } else { Op::Move { sources: files, dest: into } });
    }

    // ---- context menu -----------------------------------------------------------------------

    /// Right-click on row `i` (or on empty space when `None`): a native menu.
    pub fn context_menu(&mut self, row: Option<usize>, press: PointerPress, tx: &mut Tx) {
        if let Some(i) = row
            && !self.is_selected(i)
        {
            self.select(i);
        }
        let has = row.is_some() && !self.visible.is_empty();
        let n = if has { self.targets().len() } else { 0 };
        let sc = |s: &str| s.parse::<rune::Shortcut>().ok();
        let item = |id: &str, title: &str, on: bool, key: &str| {
            let it = PopupItem::new(id, title).enabled(on);
            match sc(key) {
                Some(k) => it.key_equivalent(k),
                None => it,
            }
        };
        let what = if n > 1 { format!("{n} Items") } else { String::new() };
        let menu = PopupMenu::new()
            .item(item("open", "Open", has, "cmd+o"))
            .item(item("reveal", reveal_label(), true, "cmd+enter"))
            .separator()
            .item(item("rename", "Rename…", has && n == 1, "cmd+shift+r"))
            .item(item("duplicate", format!("Duplicate {what}").trim_end(), has, "cmd+d"))
            .item(item("trash", &format!("Move {what} to Trash").replace("  ", " "), has, "cmd+backspace"))
            .separator()
            .item(item("copy", format!("Copy {what}").trim_end(), has, "cmd+c"))
            .item(item("cut", format!("Cut {what}").trim_end(), has, "cmd+x"))
            .item(item("paste", "Paste", true, "cmd+v"))
            .item(item("copy-path", "Copy Path", true, "cmd+shift+c"))
            .separator()
            .item(item("new-folder", "New Folder", true, "cmd+shift+n"))
            .item(item("undo", "Undo", !self.undo.is_empty(), "cmd+z"));
        tx.popup_menu(menu, press, |s: &mut State, picked, tx| {
            tx.instant();
            match picked {
                Some("open") => s.open_selected(),
                Some("reveal") => s.reveal_selected(),
                Some("rename") => s.start_rename(),
                Some("duplicate") => s.duplicate(),
                Some("trash") => s.trash_selected(),
                Some("copy") => s.copy_selected(false, tx),
                Some("cut") => s.copy_selected(true, tx),
                Some("paste") => tx.request_paste(),
                Some("copy-path") => s.copy_path(tx),
                Some("new-folder") => s.new_folder(),
                Some("undo") => s.undo_last(),
                _ => {}
            }
        });
    }
}

fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Show in Finder"
    } else if cfg!(windows) {
        "Show in Explorer"
    } else {
        "Show in File Manager"
    }
}

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
