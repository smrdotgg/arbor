//! The worker's side of what's in the Trash: emptying it ([`Op::EmptyTrash`]) and Put Back
//! ([`Op::PutBack`]). Which trashes there are, and how their records read, is
//! `trash::contents`.

use super::*;
use crate::trash::contents::{self, Item};

impl Worker<'_> {
    /// [`Op::EmptyTrash`]: the items the dialog's count found, or (when it hadn't landed) what's
    /// in the Trash now. Each is deleted as Delete Permanently deletes (only while it's still
    /// the item counted, never following a link or crossing into another disk; Esc stops it
    /// between entries), then its `.trashinfo`. On Windows the shell empties the Recycle Bin.
    pub(super) fn empty_trash(&mut self, items: Option<Vec<Item>>, out: &mut Outcome) {
        #[cfg(windows)]
        if contents::SHELL {
            self.p.items_total.store(1, Relaxed);
            match contents::shell_empty() {
                Ok(said) => out.said = Some(said),
                Err(e) => out.refused = Some(e),
            }
            return;
        }
        let items = match items {
            Some(items) => items,
            None => {
                let p = self.p;
                let Some((items, problem)) = contents::list(&contents::bins(), &|| p.cancel.load(Relaxed)) else { return };
                if items.is_empty() {
                    out.said = Some(match problem {
                        Some(p) => p.say(),
                        None => format!("The {} is already empty", crate::trash::BIN),
                    });
                    return;
                }
                items
            }
        };
        if items.is_empty() {
            out.said = Some(format!("The {} is already empty", crate::trash::BIN));
            return;
        }
        let mut dirs: Vec<PathBuf> = items.iter().filter_map(|i| i.path.parent().map(Path::to_path_buf)).collect();
        dirs.sort();
        dirs.dedup();
        out.folders.extend(dirs.into_iter().map(|d| {
            let id = noclobber::dir_id(&d);
            (d, id)
        }));
        self.p.items_total.store(items.len(), Relaxed);
        #[cfg(unix)]
        self.empty_bins(items, out);
        #[cfg(not(unix))]
        {
            let doomed = items.iter().map(|i| Doomed { path: i.path.clone(), id: i.id, dir: false }).collect();
            self.delete(doomed, out);
            for it in &items {
                if !out.removed.contains(&it.path) {
                    continue;
                }
                if let Some(info) = &it.info
                    && let Err(e) = std::fs::remove_file(info)
                    && e.kind() != ErrorKind::NotFound
                {
                    out.errors.push(format!("“{}” was deleted, but its record in the Trash couldn't be removed ({e})", name_of(&it.path)));
                }
            }
        }
    }

    /// Empty Trash on Unix: each trash's folders opened again, without following links, and
    /// only while each is still the folder counted ([`contents::reopen`]); then each item
    /// checked through them right before it goes (its identity, and on Linux its handle), and
    /// removed, with its record, relative to them: nothing is reached by a path a link could
    /// redirect.
    #[cfg(unix)]
    fn empty_bins(&mut self, items: Vec<Item>, out: &mut Outcome) {
        use std::os::unix::ffi::OsStrExt;
        let mut groups: Vec<(Arc<contents::Pin>, Vec<Item>)> = Vec::new();
        for it in items {
            match groups.iter_mut().find(|(p, _)| Arc::ptr_eq(p, &it.pin) || **p == *it.pin) {
                Some((_, v)) => v.push(it),
                None => groups.push((it.pin.clone(), vec![it])),
            }
        }
        let mounts = self.mounts().clone();
        for (pin, items) in groups {
            if self.cancelled() {
                break;
            }
            let opened = match contents::reopen(&pin) {
                Ok(o) => o,
                Err(why) => {
                    out.errors.push(format!("Nothing in “{}” was deleted: {why}", crate::fs::pretty(&pin.bin.items)));
                    self.p.items_done.fetch_add(items.len(), Relaxed);
                    continue;
                }
            };
            for it in &items {
                if self.cancelled() {
                    break;
                }
                let name = name_of(&it.path);
                self.before_act(&it.path);
                let Some(c) = it.path.file_name().and_then(|n| std::ffi::CString::new(n.as_bytes()).ok()) else { continue };
                let problem = match (it.id, crate::dirfd::ident_at(&opened.files, &c)) {
                    (_, Err(e)) if e.kind() == ErrorKind::NotFound => Some(format!("“{name}” was already gone")),
                    (None, _) => Some(format!("“{name}” couldn't be identified when it was chosen, so it was left alone")),
                    (Some(want), Ok(now))
                        if now == want && (it.handle.is_none() || crate::dirfd::handle_at(&opened.files, &c) == it.handle) =>
                    {
                        None
                    }
                    (Some(_), Ok(_)) => {
                        Some(format!("“{name}” isn't the item you chose (something else is at its path now), so it was left alone"))
                    }
                    (Some(_), Err(e)) => Some(format!("Couldn't delete “{name}”: {e}")),
                };
                if let Some(why) = problem {
                    out.errors.push(why);
                    self.p.items_done.fetch_add(1, Relaxed);
                    continue;
                }
                let files = &opened.files;
                let gone = self.remove_with(&it.path, &mounts, out, |path, mounts, go, removed| {
                    crate::delete::remove_in(files, &c, path, mounts, go, removed)
                });
                if gone && let Some(info) = &opened.info {
                    let mut rec = c.as_bytes().to_vec();
                    rec.extend_from_slice(b".trashinfo");
                    if let Ok(rec) = std::ffi::CString::new(rec)
                        && let Err(e) = crate::dirfd::unlink_at(info, &rec)
                        && e.kind() != ErrorKind::NotFound
                    {
                        out.errors.push(format!("“{name}” was deleted, but its record in the Trash couldn't be removed ({e})"));
                    }
                }
                self.p.items_done.fetch_add(1, Relaxed);
            }
        }
    }

    /// [`Op::PutBack`]: each item goes back to where its `.trashinfo` says it came from, never
    /// over anything: when that name is taken, under a free one beside it ("name 2"), which
    /// the result says. Its record goes once it's out. One whose folder is gone stays in the
    /// Trash, said so. Undoing puts them in the Trash again.
    pub(super) fn restore_from_trash(&mut self, items: Vec<PathBuf>, out: &mut Outcome) {
        self.p.items_total.store(items.len(), Relaxed);
        #[cfg(all(unix, not(target_os = "macos")))]
        let home = Some(crate::trash::home_trash());
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        let home: Option<PathBuf> = None;
        let mut steps = Vec::new();
        let mut back_in = Vec::new();
        for src in &items {
            if self.cancelled() {
                break;
            }
            let name = name_of(src);
            if let Some(why) = self.changed(src) {
                out.errors.push(why);
                self.p.items_done.fetch_add(1, Relaxed);
                continue;
            }
            match self.put_back_one(src, home.as_deref()) {
                Ok((to, wanted, kept)) => {
                    if let Some(e) = kept {
                        out.errors.push(format!(
                            "“{name}” is back in “{}”, but its record in the Trash couldn't be removed ({e})",
                            parent_of(&to)
                        ));
                    }
                    if to != wanted {
                        out.notes.push(format!(
                            "“{name}” went back as “{}”: “{}” is taken in “{}”",
                            name_of(&to),
                            name_of(&wanted),
                            crate::fs::pretty(Path::new(&parent_of(&to)))
                        ));
                    }
                    back_in.extend(to.parent().map(Path::to_path_buf));
                    out.moved.push((src.clone(), to.clone()));
                    steps.extend(self.inverse_step(to.clone(), None, false));
                    out.results.push(to);
                }
                Err(e) => out.errors.push(format!("“{name}” stayed in the Trash: {e}")),
            }
            self.p.items_done.fetch_add(1, Relaxed);
            self.tick(0);
        }
        back_in.sort();
        back_in.dedup();
        out.folders.extend(back_in.into_iter().map(|d| {
            let id = noclobber::dir_id(&d);
            (d, id)
        }));
        if !steps.is_empty() {
            out.undo = Some(Undo { what: What::PutBack, redo: false, steps });
        }
    }

    /// Puts one trashed item back: where it went, where it came from, and why its record is
    /// still there if it is.
    fn put_back_one(&mut self, src: &Path, home: Option<&Path>) -> Result<(PathBuf, PathBuf, Option<io::Error>), String> {
        let origin = contents::original(src, home)?;
        let wanted = origin.wanted.clone();
        let (Some(dir), Some(want)) = (wanted.parent(), wanted.file_name()) else { return Err("its record names no folder".into()) };
        if !dir.is_dir() {
            return Err(format!("the folder it came from, “{}”, isn't there any more", crate::fs::pretty(dir)));
        }
        for _ in 0..TRIES {
            let to = free_name(dir, want, false);
            self.before_act(&to);
            match self.relocate(src, &to) {
                Ok(()) => {
                    // Its record would list it in the Trash still.
                    let kept = origin.forget().err();
                    return Ok((to, wanted.clone(), kept));
                }
                // Taken since `free_name` looked: the next free name.
                Err(MoveError::Taken) => {}
                Err(MoveError::Failed(e)) => return Err(e),
                Err(MoveError::Split { why, .. }) => {
                    return Err(format!("copied to “{}”, but part of it couldn't be removed from the Trash ({why})", to.display()));
                }
            }
        }
        Err("every free name was taken before it could be used".into())
    }
}
