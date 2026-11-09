//! The operation worker: runs one [`Op`] on its own thread, with progress and cancel. Copies,
//! moves, trash, permanent delete and the identity re-checks right before each item; undo is
//! in `undo.rs` and batch rename in `batch.rs`.

use super::*;

pub(super) struct Worker<'a> {
    pub(super) p: &'a Progress,
    pub(super) waker: Option<&'a rune::Waker>,
    pub(super) last_wake: Instant,
    pub(super) trash: TrashFn,
    /// Each copy's, move's and trash's sources as they were when it began (or, for search
    /// results, as the walk found them): checked again right before each item is acted on, so
    /// one replaced or removed while an earlier one was being handled is left alone
    /// ([`Worker::changed`]).
    pub(super) seen: std::collections::HashMap<PathBuf, crate::fs::Ident>,
    /// `seen` came from a search's walk (its words name the search).
    pub(super) seen_by_search: bool,
    /// Where other disks are mounted, read once per operation when a recursive removal first
    /// needs it, so none crosses into one.
    pub(super) mounts: std::cell::OnceCell<crate::mounts::Mounts>,
    #[cfg(test)]
    pub(super) hooks: Hooks,
}

/// Why a move didn't happen.
pub(super) enum MoveError {
    /// Something is at the destination. Nothing changed.
    Taken,
    /// Nothing changed (a half-made copy was removed again).
    Failed(String),
    /// Across disks: everything was copied to the destination, but part of the original
    /// couldn't be removed (`why`). Both halves fingerprinted as they are now.
    Split { split: Box<Split>, why: String },
}

pub(super) enum CopyError {
    /// Something is at the destination. Nothing was made.
    Taken,
    Failed(String),
}

/// An item a copy made, and what it was copied from.
pub(super) struct Made {
    pub(super) src: PathBuf,
    pub(super) dst: PathBuf,
    pub(super) dir: bool,
    /// For a move, the source file or link as it was copied, so the delete afterwards removes
    /// it only if it hasn't changed since. Folders are removed only once empty.
    pub(super) was: Option<Print>,
    /// `dst` right after the copy made it, so cleaning up a failed or cancelled copy removes it
    /// only while it's still that (see [`unmake`]). `None` if it couldn't be read back.
    pub(super) made: Option<Print>,
}

impl<'a> Worker<'a> {
    pub(super) fn new(p: &'a Progress, waker: Option<&'a rune::Waker>, trash: TrashFn) -> Self {
        Worker {
            p,
            waker,
            last_wake: Instant::now(),
            trash,
            seen: std::collections::HashMap::new(),
            seen_by_search: false,
            mounts: std::cell::OnceCell::new(),
            #[cfg(test)]
            hooks: Hooks::default(),
        }
    }

    /// The mount table (read on first use, then kept for this operation).
    pub(super) fn mounts(&self) -> &crate::mounts::Mounts {
        self.mounts.get_or_init(|| {
            #[cfg(test)]
            if let Some(m) = &self.hooks.mounts {
                return m.clone();
            }
            crate::mounts::Mounts::read()
        })
    }

    pub(super) fn cancelled(&self) -> bool {
        self.p.cancel.load(Relaxed)
    }

    /// Pushes progress to the UI at most ~20 times a second. The worker wakes the window;
    /// the window never polls.
    pub(super) fn tick(&mut self, bytes: u64) {
        self.p.bytes_done.fetch_add(bytes, Relaxed);
        if self.last_wake.elapsed() >= Duration::from_millis(50) {
            self.last_wake = Instant::now();
            if let Some(w) = self.waker {
                w();
            }
        }
    }

    #[cfg(test)]
    pub(super) fn before_act(&mut self, dest: &Path) {
        if let Some(h) = &mut self.hooks.before_act {
            h(dest);
        }
    }

    #[cfg(not(test))]
    #[inline(always)]
    pub(super) fn before_act(&mut self, _: &Path) {}

    #[cfg(test)]
    pub(super) fn cancel_hook(&self, at: &Path) {
        if self.hooks.cancel_at.as_ref().is_some_and(|f| f(at)) {
            self.p.cancel.store(true, Relaxed);
        }
    }

    #[cfg(not(test))]
    #[inline(always)]
    pub(super) fn cancel_hook(&self, _: &Path) {}

    #[cfg(test)]
    pub(super) fn after_made(&mut self, dst: &Path) {
        if let Some(h) = &mut self.hooks.after_made {
            h(dst);
        }
    }

    #[cfg(not(test))]
    #[inline(always)]
    pub(super) fn after_made(&mut self, _: &Path) {}

    #[cfg(test)]
    pub(super) fn copied(&mut self, dst: &Path) {
        if let Some(h) = &mut self.hooks.copied {
            h(dst);
        }
    }

    #[cfg(not(test))]
    #[inline(always)]
    pub(super) fn copied(&mut self, _: &Path) {}

    pub(super) fn rename_excl(&self, from: &Path, to: &Path) -> io::Result<()> {
        #[cfg(test)]
        if self.hooks.cross_device {
            return Err(io::Error::from(ErrorKind::CrossesDevices));
        }
        #[cfg(test)]
        if self.hooks.fail_rename.as_ref().is_some_and(|f| f(to)) {
            return Err(io::Error::new(ErrorKind::PermissionDenied, "permission denied"));
        }
        noclobber::rename(from, to)
    }

    /// Removes one file or link, or one empty folder.
    pub(super) fn remove_one(&self, p: &Path, dir: bool) -> io::Result<()> {
        #[cfg(test)]
        if self.hooks.fail_remove.as_ref().is_some_and(|f| f(p)) {
            return Err(io::Error::new(ErrorKind::PermissionDenied, "permission denied"));
        }
        if dir { std::fs::remove_dir(p) } else { std::fs::remove_file(p) }
    }

    /// [`Worker::run`], first checking that a copy's or move's destination is still the folder
    /// with identity `dest_id`.
    pub(super) fn run_into(&mut self, op: Op, dest_id: Option<noclobber::FileId>) -> Outcome {
        if let (Some(want), Op::Copy { dest, .. } | Op::Move { dest, .. }) = (dest_id, &op)
            && noclobber::dir_id(dest) != Some(want)
        {
            let why = if dest.symlink_metadata().is_ok() {
                "is another folder now (the one there was moved or replaced)"
            } else {
                "isn't there any more"
            };
            return Outcome {
                results: Vec::new(),
                undo: None,
                inverse: None,
                errors: Vec::new(),
                refused: Some(format!("Nothing was pasted: “{}” {why}", name_of(dest))),
                cancelled: false,
                moved: Vec::new(),
                removed: Vec::new(),
                no_trash: Vec::new(),
                no_trash_why: None,
                partly: None,
                said: None,
                notes: Vec::new(),
                quiet: false,
                canon: Vec::new(),
                folders: Vec::new(),
            };
        }
        self.run(op)
    }

    pub(super) fn run(&mut self, op: Op) -> Outcome {
        // Identities from before: an operation never replaces the folders it works in.
        let folders: Vec<(PathBuf, Option<noclobber::FileId>)> = op
            .folders()
            .into_iter()
            .map(|p| {
                let id = noclobber::dir_id(&p);
                (p, id)
            })
            .collect();
        let canon = folders.iter().filter_map(|(p, _)| Some((p.clone(), std::fs::canonicalize(p).ok()?))).collect();
        if let Op::Copy { sources, .. } | Op::Move { sources, .. } | Op::Trash { sources } | Op::PutBack { items: sources } = &op {
            for src in sources {
                if !self.seen.contains_key(src)
                    && let Some(id) = crate::fs::ident_at(src)
                {
                    self.seen.insert(src.clone(), id);
                }
            }
        }
        let mut out = Outcome {
            results: Vec::new(),
            undo: None,
            inverse: None,
            errors: Vec::new(),
            refused: None,
            cancelled: false,
            moved: Vec::new(),
            removed: Vec::new(),
            no_trash: Vec::new(),
            no_trash_why: None,
            partly: None,
            said: None,
            notes: Vec::new(),
            quiet: false,
            canon,
            folders,
        };
        match op {
            Op::Copy { sources, dest } => {
                self.p.items_total.store(sources.len(), Relaxed);
                self.p.bytes_total.store(sources.iter().map(|s| tree_size(s)).sum(), Relaxed);
                let mut steps = Vec::new();
                for src in &sources {
                    if self.cancelled() {
                        break;
                    }
                    if let Some(why) = self.changed(src) {
                        out.errors.push(why);
                        self.p.items_done.fetch_add(1, Relaxed);
                        continue;
                    }
                    match self.copy_one(src, &dest) {
                        Ok((to, print)) => {
                            out.results.push(to.clone());
                            steps.push(Step { at: to, print, back: None, split: None });
                        }
                        Err(e) => out.errors.push(format!("{}: {e}", name_of(src))),
                    }
                    self.p.items_done.fetch_add(1, Relaxed);
                }
                let dup = sources.iter().all(|s| s.parent() == Some(dest.as_path()));
                if !steps.is_empty() {
                    out.undo = Some(Undo { what: if dup { What::Duplicate } else { What::Copy }, redo: false, steps });
                }
            }
            Op::Move { sources, dest } => {
                self.p.items_total.store(sources.len(), Relaxed);
                let mut pairs = Vec::new();
                for src in &sources {
                    if self.cancelled() {
                        break;
                    }
                    if let Some(why) = self.changed(src) {
                        out.errors.push(why);
                        self.p.items_done.fetch_add(1, Relaxed);
                        continue;
                    }
                    match self.move_one(src, &dest) {
                        Ok(to) => {
                            if &to != src {
                                pairs.push((to.clone(), Some(src.clone())));
                                out.moved.push((src.clone(), to.clone()));
                            }
                            out.results.push(to);
                        }
                        Err(e) => out.errors.push(format!("{}: {e}", name_of(src))),
                    }
                    self.p.items_done.fetch_add(1, Relaxed);
                }
                out.undo = Undo::record_or_say(What::Move, pairs, &mut out.errors);
            }
            Op::Trash { sources } => {
                self.p.items_total.store(sources.len(), Relaxed);
                let mut pairs = Vec::new();
                let mut restorable = true;
                for src in &sources {
                    if self.cancelled() {
                        break;
                    }
                    if let Some(why) = self.changed(src) {
                        out.errors.push(why);
                        self.p.items_done.fetch_add(1, Relaxed);
                        continue;
                    }
                    let p = self.p;
                    match (self.trash)(src, &|| p.cancel.load(Relaxed)) {
                        Ok(Some(at)) => {
                            pairs.push((at, Some(src.clone())));
                            out.removed.push(src.clone());
                        }
                        Ok(None) => {
                            restorable = false;
                            out.removed.push(src.clone());
                        }
                        // Left where it was: gathered for one "delete permanently?" question.
                        Err(TrashError::NoTrash(why)) => {
                            out.no_trash_why.get_or_insert(why);
                            out.no_trash.push(Doomed::now(src));
                        }
                        Err(TrashError::DeletedForGood) => {
                            restorable = false;
                            out.removed.push(src.clone());
                            out.errors.push(format!("{}: {}", name_of(src), TrashError::DeletedForGood));
                        }
                        Err(TrashError::Failed(e)) => out.errors.push(format!("{}: {e}", name_of(src))),
                        // Untouched; the loop sees the cancel.
                        Err(TrashError::Cancelled) => {}
                    }
                    self.p.items_done.fetch_add(1, Relaxed);
                    self.tick(0);
                }
                if restorable {
                    out.undo = Undo::record_or_say(What::Trash, pairs, &mut out.errors);
                }
            }
            Op::Delete { items } => self.delete(items, &mut out),
            Op::EmptyTrash { items } => self.empty_trash(items, &mut out),
            Op::PutBack { items } => self.restore_from_trash(items, &mut out),
            Op::RenameOne { from, to } => {
                self.p.items_total.store(1, Relaxed);
                out.quiet = true;
                match rename(&from, &to) {
                    // No result to select: each pane keeps its own selection, which followed the
                    // item when the window showed the new name.
                    Ok(()) => {
                        out.moved.push((from.clone(), to.clone()));
                        out.undo = Undo::renamed(to.clone(), from);
                        if out.undo.is_none() {
                            out.said = Some(format!("Renamed, but it can't be undone: “{}” couldn't be read right after", name_of(&to)));
                        }
                    }
                    Err(e) => out.refused = Some(e),
                }
            }
            Op::NewFolder { dir } => {
                self.p.items_total.store(1, Relaxed);
                out.quiet = true;
                match new_folder(&dir, "untitled folder") {
                    Ok(path) => {
                        out.undo = Undo::new_folder(path.clone());
                        out.results.push(path);
                    }
                    Err(e) => out.refused = Some(format!("Couldn't make a folder: {e}")),
                }
            }
            Op::Rename { items } => self.batch_rename(items, &mut out),
            Op::Undo(undo) if undo.what == What::BatchRename => self.undo_batch_rename(undo, &mut out),
            Op::Undo(undo) => self.undo(undo, &mut out),
            Op::Checked { expect, op } => return self.run_checked(&expect, *op),
        }
        out.cancelled = self.cancelled();
        out
    }

    /// [`Op::Checked`]: leaves out each item that isn't the one the search found, says why,
    /// and runs the rest.
    pub(super) fn run_checked(&mut self, expect: &[(PathBuf, crate::fs::Ident)], op: Op) -> Outcome {
        let mut why = Vec::new();
        let mut left_out = std::collections::HashSet::new();
        for (path, id) in expect {
            let name = name_of(path);
            let problem = match path.symlink_metadata() {
                Ok(m) if crate::fs::ident(&m) == Some(*id) => continue,
                Ok(_) => format!("“{name}” isn't the item the search found (something else is at its path now), so it was left alone"),
                Err(_) => format!("“{name}” isn't where the search found it any more, so it was left alone"),
            };
            why.push(problem);
            left_out.insert(path.as_path());
        }
        let (op, any) = op.without(&left_out);
        // Each item is checked again right before it's acted on (`Worker::changed`).
        self.seen.extend(expect.iter().cloned());
        self.seen_by_search = true;
        if !any {
            let more = if why.len() > 1 { format!(" (and {} more)", why.len() - 1) } else { String::new() };
            return Outcome {
                results: Vec::new(),
                undo: None,
                inverse: None,
                errors: Vec::new(),
                refused: Some(format!("Nothing was done: {}{more}", why.first().cloned().unwrap_or_default())),
                cancelled: false,
                moved: Vec::new(),
                removed: Vec::new(),
                no_trash: Vec::new(),
                no_trash_why: None,
                partly: None,
                said: None,
                notes: Vec::new(),
                quiet: false,
                canon: Vec::new(),
                folders: Vec::new(),
            };
        }
        let mut out = self.run(op);
        why.append(&mut out.errors);
        out.errors = why;
        out
    }

    /// Why `src` isn't the item it was when the operation began (or when the search found
    /// it), checked right before it's acted on; `None` when it still is, or there's nothing to
    /// compare with. One lstat.
    pub(super) fn changed(&self, src: &Path) -> Option<String> {
        let want = self.seen.get(src)?;
        let name = name_of(src);
        Some(match (src.symlink_metadata(), self.seen_by_search) {
            (Ok(m), _) if crate::fs::ident(&m) == Some(*want) => return None,
            (Ok(_), true) => format!("“{name}” isn't the item the search found (something else is at its path now), so it was left alone"),
            (Err(_), true) => format!("“{name}” isn't where the search found it any more, so it was left alone"),
            (Ok(_), false) => format!("“{name}” was replaced while the operation ran, so it was left alone"),
            (Err(_), false) => format!("“{name}” was moved or deleted while the operation ran, so it was left alone"),
        })
    }

    /// [`Op::Delete`]: each item is checked right before it goes (an item replaced or gone since
    /// the user chose it is left alone and said so), then removed whole. A cancel stops at once:
    /// what's gone stays gone, and a folder it stopped in is partly deleted, which the outcome
    /// says ([`Outcome::partly`]).
    pub(super) fn delete(&mut self, items: Vec<Doomed>, out: &mut Outcome) {
        self.p.items_total.store(items.len(), Relaxed);
        for d in &items {
            if self.cancelled() {
                break;
            }
            let name = name_of(&d.path);
            self.before_act(&d.path);
            let problem = match d.path.symlink_metadata() {
                _ if d.path.parent().is_none() => Some(format!("“{}” can't be deleted", d.path.display())),
                Err(_) => Some(format!("“{name}” was already gone")),
                // Nothing to check it against: whatever is there now can't be told from the
                // item chosen, so it's never deleted on a guess.
                Ok(_) if d.id.is_none() => Some(format!("“{name}” couldn't be identified when it was chosen, so it was left alone")),
                Ok(m) if crate::fs::ident(&m) != d.id => {
                    Some(format!("“{name}” isn't the item you chose (something else is at its path now), so it was left alone"))
                }
                Ok(_) => None,
            };
            if let Some(why) = problem {
                out.errors.push(why);
                self.p.items_done.fetch_add(1, Relaxed);
                continue;
            }
            let mut removed = 0;
            let mounts = self.mounts().clone();
            let w = &mut *self;
            let mut go = |at: &Path| {
                w.cancel_hook(at);
                w.tick(0);
                !w.cancelled()
            };
            match crate::delete::remove_tree(&d.path, &mounts, &mut go, &mut removed) {
                Ok(()) => out.removed.push(d.path.clone()),
                Err(crate::delete::Halt::Cancelled) => {
                    if removed > 0 {
                        out.partly = Some(d.path.clone());
                    }
                }
                Err(halt) => {
                    let Some((at, why)) = halt.why() else { continue };
                    let shown = d.path.parent().and_then(|p| at.strip_prefix(p).ok()).unwrap_or(at).display().to_string();
                    let first = if matches!(halt, crate::delete::Halt::Failed { .. }) { "Couldn't delete" } else { "Skipped" };
                    if removed == 0 {
                        out.errors.push(format!("{first} “{shown}”: {why}"));
                    } else {
                        // Truthful counts: what went, and what's still there (counted on this
                        // worker, not entering mounts; left out if a cancel stops the count, and
                        // never a guess when part of it can't be read).
                        let left = left_note(crate::delete::count_left(&d.path, &mounts, &mut || !self.cancelled()), &d.path);
                        out.errors.push(format!(
                            "Partly deleted “{}”: stopped at “{shown}” ({why}); {removed} item{} deleted{left}",
                            name_of(&d.path),
                            if removed == 1 { "" } else { "s" }
                        ));
                    }
                }
            }
            self.p.items_done.fetch_add(1, Relaxed);
        }
    }

    /// Copies `src` into `dest_dir` under a free name; returns where it went and its fingerprint.
    pub(super) fn copy_one(&mut self, src: &Path, dest_dir: &Path) -> Result<(PathBuf, Print), String> {
        let name = src.file_name().ok_or("no file name")?;
        if src.is_dir() && dest_dir.starts_with(src) {
            return Err("can't copy a folder into itself".into());
        }
        let same_dir = src.parent() == Some(dest_dir);
        for _ in 0..TRIES {
            let to = free_name(dest_dir, name, same_dir);
            self.before_act(&to);
            let mut tree = Some(Tree::default());
            let mut made = Vec::new();
            match self.copy_tree(src, &to, Path::new(""), &mut tree, &mut made, false) {
                Ok(()) => {
                    // The record undo checks against: a file as the copy knew it (by its handle,
                    // never by its name, which another app may have taken over while the copy
                    // stalled); a folder read now that its contents are in, but only while it's
                    // still the folder the copy made.
                    let first = made.first().and_then(|m| m.made.clone());
                    let print = match first {
                        Some(file) if file.kind != Kind::Folder => file,
                        made_dir => {
                            let mut print = Print::of(&to, &to.symlink_metadata().map_err(|e| e.to_string())?);
                            if made_dir.is_none_or(|d| d.id.is_none() || d.id != print.id) {
                                let why = format!("something else took the place of “{}” during the copy", name_of(&to));
                                return Err(with_kept(why, unmake(&made)));
                            }
                            print.tree = tree;
                            print
                        }
                    };
                    return Ok((to, print));
                }
                // Taken since `free_name` looked: try the next free name.
                Err(CopyError::Taken) => {}
                Err(CopyError::Failed(e)) => {
                    // Don't leave a half-made copy behind.
                    return Err(with_kept(e, unmake(&made)));
                }
            }
        }
        Err("every free name was taken before it could be used".into())
    }

    /// Copies `src` to `dst`, never over anything. Each item made is pushed to `made` as it
    /// lands; with `for_move`, with the source's fingerprint. With `tree`, adds each item made
    /// below the top (`rel` is its path from the top), which costs one `lstat` per item.
    pub(super) fn copy_tree(
        &mut self,
        src: &Path,
        dst: &Path,
        rel: &Path,
        tree: &mut Option<Tree>,
        made: &mut Vec<Made>,
        for_move: bool,
    ) -> Result<(), CopyError> {
        if self.cancelled() {
            return Err(CopyError::Failed("cancelled".into()));
        }
        let top = rel.as_os_str().is_empty();
        let fail = |e: io::Error| match e.kind() {
            ErrorKind::AlreadyExists if top => CopyError::Taken,
            ErrorKind::AlreadyExists => CopyError::Failed(format!("something else appeared at “{}” during the copy", dst.display())),
            _ => CopyError::Failed(e.to_string()),
        };
        let meta = src.symlink_metadata().map_err(fail)?;
        let ft = meta.file_type();
        let was = (for_move && !ft.is_dir()).then(|| Print::of(src, &meta));
        let print = |dst: &Path| dst.symlink_metadata().ok().map(|m| Print::of(dst, &m));
        if ft.is_symlink() {
            let target = std::fs::read_link(src).map_err(fail)?;
            make_symlink(&target, dst, src.is_dir()).map_err(fail)?;
            made.push(Made { src: src.into(), dst: dst.into(), dir: false, was, made: print(dst) });
        } else if ft.is_dir() {
            std::fs::create_dir(dst).map_err(fail)?;
            let _ = std::fs::set_permissions(dst, meta.permissions());
            made.push(Made { src: src.into(), dst: dst.into(), dir: true, was, made: print(dst) });
            self.after_made(dst);
            for de in std::fs::read_dir(src).map_err(fail)? {
                let de = de.map_err(fail)?;
                let name = de.file_name();
                let rel = if tree.is_some() { rel.join(&name) } else { rel.join("_") };
                self.copy_tree(&de.path(), &dst.join(&name), &rel, tree, made, for_move)?;
            }
        } else {
            // Fingerprinted from the handle that made it, not by name: the copy may have stalled
            // on a slow source while another app put a file of its own at `dst`, which cleanup
            // must then leave alone.
            let file = noclobber::copy_file(src, dst).map_err(fail)?;
            self.copied(dst);
            made.push(Made { src: src.into(), dst: dst.into(), dir: false, was, made: Some(Print::with_id(&file.meta, file.id)) });
            self.tick(meta.len());
            self.after_made(dst);
        }
        // A folder's own entry goes in after its contents, once its mtime has settled.
        if let Some(t) = tree
            && !top
        {
            t.add(rel, &Print::of(dst, &dst.symlink_metadata().map_err(fail)?));
        }
        Ok(())
    }

    pub(super) fn move_one(&mut self, src: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
        let name = src.file_name().ok_or("no file name")?;
        if src.parent() == Some(dest_dir) {
            return Ok(src.to_path_buf()); // already there
        }
        if src.is_dir() && dest_dir.starts_with(src) {
            return Err("can't move a folder into itself".into());
        }
        for _ in 0..TRIES {
            let to = free_name(dest_dir, name, false);
            self.before_act(&to);
            match self.relocate(src, &to) {
                Ok(()) => return Ok(to),
                // Taken since `free_name` looked: try the next free name.
                Err(MoveError::Taken) => {}
                Err(MoveError::Failed(e)) => return Err(e),
                Err(MoveError::Split { why, .. }) => {
                    return Err(format!("copied to “{}”, but part of the original couldn't be removed ({why})", to.display()));
                }
            }
        }
        Err("every free name was taken before it could be used".into())
    }

    /// Moves `from` to `to`, never over anything: a no-replace rename, or across disks a copy
    /// and then a delete of exactly what was copied.
    pub(super) fn relocate(&mut self, from: &Path, to: &Path) -> Result<(), MoveError> {
        match self.rename_excl(from, to) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => Err(MoveError::Taken),
            Err(e) if noclobber::is_cross_device(&e) => self.copy_then_remove(from, to),
            Err(e) => Err(MoveError::Failed(e.to_string())),
        }
    }

    /// Across disks: copies `from` to `to`, then removes the original. If the copy fails, what
    /// it made is removed and nothing has changed. If the delete fails partway, everything is at
    /// `to` and the rest stays at `from`: [`MoveError::Split`] says so, fingerprinting both.
    pub(super) fn copy_then_remove(&mut self, from: &Path, to: &Path) -> Result<(), MoveError> {
        self.p.bytes_total.fetch_add(tree_size(from), Relaxed);
        let mut tree = Some(Tree::default());
        let mut made = Vec::new();
        match self.copy_tree(from, to, Path::new(""), &mut tree, &mut made, true) {
            Ok(()) => {}
            Err(CopyError::Taken) => return Err(MoveError::Taken),
            Err(CopyError::Failed(e)) => return Err(MoveError::Failed(with_kept(e, unmake(&made)))),
        }
        let Err(why) = self.remove_sources(&made) else { return Ok(()) };
        let copy = match to.symlink_metadata() {
            Ok(m) => Print { tree: tree.filter(|_| m.is_dir()), ..Print::of(to, &m) },
            Err(e) => return Err(MoveError::Failed(format!("copied, then the copy vanished: {e}"))),
        };
        self.split(from, copy, why)
    }

    /// After a failed delete, fingerprints what's left at `from` so a retry can finish.
    pub(super) fn split(&self, from: &Path, copy: Print, why: String) -> Result<(), MoveError> {
        match self.print_all(from) {
            Ok(rest) => Err(MoveError::Split { split: Box::new(Split { copy, rest }), why }),
            // The rest is gone after all.
            Err(e) if is_gone(&e) => Ok(()),
            // Unreadable: a fingerprint nothing matches, so a retry refuses and says where things are.
            Err(e) => Err(MoveError::Split { split: Box::new(Split { copy, rest: unmatchable() }), why: format!("{why}; {e}") }),
        }
    }

    /// Removes the originals of a cross-disk move, deepest first: each file or link only if
    /// it's unchanged since it was copied, each folder only once it's empty. Anything that
    /// changed or appeared meanwhile stays. Says what couldn't be removed.
    pub(super) fn remove_sources(&self, made: &[Made]) -> Result<(), String> {
        let (mut kept, mut first) = (0, None);
        // The first item made is the moved item itself: nothing on another disk mounted inside
        // it is removed (it was copied; the original stays where it's mounted).
        let fence = made.first().map(|m| crate::mounts::Fence::new(&m.src, self.mounts()));
        for m in made.iter().rev() {
            if let (Some(f), Ok(meta)) = (&fence, m.src.symlink_metadata())
                && f.crosses(&m.src, &meta)
            {
                // Counted and named once, at its mount point (what's in it comes first, deepest
                // first, and is left alone without a word).
                let parent_crosses = m.src.parent().and_then(|p| p.symlink_metadata().ok().map(|pm| f.crosses(p, &pm)));
                if parent_crosses == Some(false) {
                    kept += 1;
                    first.get_or_insert_with(|| format!("“{}”: another disk is mounted there", m.src.display()));
                }
                continue;
            }
            let removed = match &m.was {
                Some(was) => match m.src.symlink_metadata() {
                    Ok(now) if Print::of(&m.src, &now) == *was => self.remove_one(&m.src, false),
                    Ok(_) => Err(io::Error::other("it changed during the move")),
                    Err(e) => Err(e),
                },
                None => self.remove_one(&m.src, m.dir),
            };
            match removed {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                // A folder that keeps something kept below it: that one's already counted.
                Err(_) if m.dir && first.is_some() => {}
                Err(e) => {
                    kept += 1;
                    first.get_or_insert_with(|| format!("“{}”: {e}", m.src.display()));
                }
            }
        }
        match first {
            None => Ok(()),
            Some(f) if kept == 1 => Err(f),
            Some(f) => Err(format!("{f}, and {} more", kept - 1)),
        }
    }

    /// Finishes a split step: removes what's left of the original at `at` (checked unchanged
    /// just before), deepest first, folders only once empty.
    pub(super) fn remove_rest(&mut self, at: &Path, copy: &Print) -> Result<(), MoveError> {
        let mut kept = None;
        let fence = crate::mounts::Fence::new(at, self.mounts());
        self.remove_tree_checked(at, &fence, &mut kept);
        match kept {
            None => Ok(()),
            Some(why) => self.split(at, copy.clone(), why),
        }
    }

    /// Only an item that's really gone counts as removed: one that can't be read (its folder
    /// lost its permissions, say) is kept, and the step's record with it. Never enters or
    /// removes a mount point (`fence`).
    pub(super) fn remove_tree_checked(&self, p: &Path, fence: &crate::mounts::Fence, kept: &mut Option<String>) {
        let keep = |kept: &mut Option<String>, e: io::Error| {
            kept.get_or_insert_with(|| format!("“{}”: {e}", p.display()));
        };
        let m = match p.symlink_metadata() {
            Ok(m) => m,
            Err(e) if is_gone(&e) => return,
            Err(e) => return keep(kept, e),
        };
        if fence.crosses(p, &m) {
            kept.get_or_insert_with(|| format!("“{}”: another disk is mounted there", p.display()));
            return;
        }
        if m.is_dir() {
            match std::fs::read_dir(p) {
                Ok(rd) => {
                    for de in rd {
                        match de {
                            Ok(de) => self.remove_tree_checked(&de.path(), fence, kept),
                            Err(e) => keep(kept, e),
                        }
                    }
                }
                Err(e) if is_gone(&e) => return,
                Err(e) => keep(kept, e),
            }
        }
        if let Err(e) = self.remove_one(p, m.is_dir())
            && e.kind() != ErrorKind::NotFound
            && !(m.is_dir() && kept.is_some())
        {
            *kept = Some(format!("“{}”: {e}", p.display()));
        }
    }
}
