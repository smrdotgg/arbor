//! Undo and redo: the record of how to reverse a finished operation ([`Undo`]), and the worker's
//! side, which checks every step against its fingerprint before touching anything, then checks
//! each again right before its own step.

use super::*;

/// How to reverse a finished operation: each step's item, as it was left, goes back to where it
/// came from or (for what the operation made) to the Trash.
///
/// A redo is the same record pointing the other way: undoing a step yields its inverse (a move
/// back, a trashed item restored from where the Trash put it, a restored item trashed again),
/// fingerprinted as the undo left it, so redoing refuses on the same terms as undoing does
/// ([`Outcome::inverse`]). `redo` only says which way it points, for its words and for where
/// its steps' items are (see [`Undo::in_trash`]).
#[derive(Debug, Clone)]
pub struct Undo {
    pub what: What,
    pub redo: bool,
    pub(super) steps: Vec<Step>,
}

#[derive(Debug, Clone)]
pub(super) struct Step {
    /// Where the operation left the item.
    pub(super) at: PathBuf,
    /// What it looked like right after.
    pub(super) print: Print,
    /// Where it goes back to; `None` trashes it (a copy, duplicate or new folder).
    pub(super) back: Option<PathBuf>,
    /// An earlier try at this step moved it back across disks (copy, then delete) and the
    /// delete didn't finish.
    pub(super) split: Option<Split>,
}

/// A move back across disks that copied everything to `back` but couldn't remove all of the
/// original from `at`. Both halves are fingerprinted as that try left them, so a retry can
/// finish removing the rest only while neither has changed, and otherwise says where things are.
#[derive(Debug, Clone)]
pub(super) struct Split {
    /// The complete copy at `back`.
    pub(super) copy: Print,
    /// What's still at `at`.
    pub(super) rest: Print,
}

impl Undo {
    /// Fingerprints each `(at, back)` as it is now. An item that can't be read (gone already)
    /// can't be undone: it's left out, and said so in `errors`, never silently.
    pub(super) fn record_or_say(
        what: What,
        items: impl IntoIterator<Item = (PathBuf, Option<PathBuf>)>,
        errors: &mut Vec<String>,
    ) -> Option<Undo> {
        let mut steps = Vec::new();
        for (at, back) in items {
            match at.symlink_metadata() {
                Ok(m) => steps.push(Step { print: Print::of(&at, &m), at, back, split: None }),
                Err(e) => errors.push(format!("“{}” can't be undone: it couldn't be read right after ({e})", name_of(&at))),
            }
        }
        (!steps.is_empty()).then_some(Undo { what, redo: false, steps })
    }

    /// Whether its steps' items are in the Trash, going back out of it: undoing a trash, or
    /// redoing what an undo trashed (a copy, a duplicate, a new folder).
    pub(super) fn in_trash(&self) -> bool {
        match self.what {
            What::Trash => !self.redo,
            What::Copy | What::Duplicate | What::NewFolder => self.redo,
            What::Rename | What::BatchRename | What::Move => false,
        }
    }

    /// "undo" or "redo", for its messages.
    pub(super) fn word(&self) -> &'static str {
        if self.redo { "redo" } else { "undo" }
    }

    /// [`Undo::record_or_say`] for one item done on the window's thread: `None` (with nothing
    /// to undo) when it can't be read.
    pub(super) fn record(what: What, items: impl IntoIterator<Item = (PathBuf, Option<PathBuf>)>) -> Option<Undo> {
        Undo::record_or_say(what, items, &mut Vec::new())
    }

    /// A rename that just happened (`from` is now `to`).
    pub fn renamed(to: PathBuf, from: PathBuf) -> Option<Undo> {
        Undo::record(What::Rename, [(to, Some(from))])
    }

    /// A folder just made, empty.
    pub fn new_folder(path: PathBuf) -> Option<Undo> {
        let mut u = Undo::record(What::NewFolder, [(path, None)])?;
        u.steps[0].print.tree = Some(Tree::default());
        Some(u)
    }
}

impl Worker<'_> {
    /// Checks every step first and changes nothing unless all pass (no partial undo of a
    /// changed disk). Then undoes them one by one, checking each again right before its own
    /// step, and keeps the steps that don't go back in `out.undo`.
    pub(super) fn undo(&mut self, undo: Undo, out: &mut Outcome) {
        self.p.items_total.store(undo.steps.len(), Relaxed);
        let trashed = undo.in_trash();
        let problems: Vec<String> = undo.steps.iter().filter_map(|s| self.check(trashed, s).err()).collect();
        if self.cancelled() {
            out.undo = Some(undo);
            return;
        }
        if let Some(first) = problems.first() {
            let more = if problems.len() > 1 { format!(" (and {} more)", problems.len() - 1) } else { String::new() };
            out.refused = Some(format!("Can't {} the {}: {first}{more}. Nothing was changed.", undo.word(), undo.what.noun()));
            out.undo = Some(undo);
            return;
        }
        let word = undo.word();
        let Undo { what, redo, steps } = undo;
        // With one step, the check above ran just now; with more, the others' checks (walks,
        // possibly) ran in between, so each is checked again right before it's undone.
        let recheck = steps.len() > 1;
        let (mut left, mut inverse) = (Vec::new(), Vec::new());
        for step in steps {
            if self.cancelled() {
                left.push(step);
                continue;
            }
            if recheck && let Err(why) = self.check(trashed, &step) {
                out.errors.push(format!("{}: {why}", if word == "redo" { "Not redone" } else { "Not undone" }));
                left.push(step);
                continue;
            }
            if let Some(kept) = self.undo_step(trashed, word, step, out, &mut inverse) {
                left.push(kept);
            }
            self.p.items_done.fetch_add(1, Relaxed);
            self.tick(0);
        }
        if !left.is_empty() {
            out.undo = Some(Undo { what, redo, steps: left });
        }
        if !inverse.is_empty() {
            out.inverse = Some(Undo { what, redo: !redo, steps: inverse });
        }
    }

    /// The step that reverses one that just went, read as the disk is now: an item that went
    /// back to `at` (`back: Some`), or one that should go to the Trash again (`back: None`,
    /// fingerprinted with everything inside, as a copy's undo is). `deep`: fingerprint
    /// everything inside for a `back: Some` step too (an item that went to the Trash, which
    /// redo takes back out: it may be edited there). `None` when it can't be read back (then it
    /// can't be reversed again, which loses nothing).
    pub(super) fn inverse_step(&self, at: PathBuf, back: Option<PathBuf>, deep: bool) -> Option<Step> {
        let print = if back.is_none() || deep { self.print_all(&at).ok()? } else { Print::of(&at, &at.symlink_metadata().ok()?) };
        Some(Step { at, print, back, split: None })
    }

    /// Undoes (or redoes) one checked step. Hands it back (updated if it got halfway) when it
    /// didn't go; when it went, adds the step that reverses it to `inverse`. `trashed`: the
    /// item is in the Trash and goes back out of it ([`Undo::in_trash`]).
    pub(super) fn undo_step(&mut self, trashed: bool, word: &str, step: Step, out: &mut Outcome, inverse: &mut Vec<Step>) -> Option<Step> {
        let name = name_of(&step.at);
        let Some(back) = step.back.clone() else {
            self.before_act(&step.at);
            let p = self.p;
            return match (self.trash)(&step.at, &|| p.cancel.load(Relaxed)) {
                Ok(put) => {
                    // Where the Trash put it, when it says (the Recycle Bin doesn't): redo
                    // takes it back out from there.
                    inverse.extend(put.and_then(|put| self.inverse_step(put, Some(step.at.clone()), true)));
                    out.removed.push(step.at.clone());
                    out.results.push(step.at);
                    None
                }
                Err(e) => {
                    out.errors.push(format!("{name}: {e}"));
                    Some(step)
                }
            };
        };
        self.before_act(&back);
        let moved = match &step.split {
            // An earlier try copied it back across disks; remove what's left of the original.
            Some(split) => self.remove_rest(&step.at, &split.copy),
            None => self.relocate(&step.at, &back),
        };
        match moved {
            Ok(()) => {
                if trashed {
                    crate::trash::forget(&step.at);
                }
                // Out of the Trash, reversing means trashing it again; otherwise moving it back.
                let to = (!trashed).then(|| step.at.clone());
                inverse.extend(self.inverse_step(back.clone(), to, false));
                out.moved.push((step.at.clone(), back.clone()));
                out.results.push(back);
                None
            }
            Err(MoveError::Taken) => {
                out.errors.push(format!("“{}” was taken just now, so “{name}” stayed where it is", back.display()));
                Some(step)
            }
            Err(MoveError::Failed(e)) => {
                out.errors.push(format!("{name}: {e}"));
                Some(step)
            }
            Err(MoveError::Split { split, why }) => {
                out.errors.push(format!(
                    "“{name}” is back in “{}”, but part of the original couldn't be removed from “{}” ({why}). {} again to remove the rest once that's fixed",
                    parent_of(&back),
                    parent_of(&step.at),
                    if word == "redo" { "Redo" } else { "Undo" },
                ));
                Some(Step { split: Some(*split), ..step })
            }
        }
    }

    /// Whether `step` can be undone: its item is still exactly as the operation left it, and
    /// where it goes back to is free. Says what changed when not.
    pub(super) fn check(&self, trashed: bool, step: &Step) -> Result<(), String> {
        let name = format!("“{}”", name_of(&step.at));
        if let (Some(split), Some(back)) = (&step.split, &step.back) {
            return self.check_split(&name, step, split, back);
        }
        match self.diff(&step.at, &step.print) {
            None => {}
            Some(Diff::Gone) if trashed => return Err(format!("{name} is no longer in the Trash")),
            Some(Diff::Gone) if step.at.parent().is_some_and(|p| p.symlink_metadata().is_err()) => {
                return Err(format!("the folder {name} was in is gone (moved, deleted, or its drive was ejected)"));
            }
            Some(d) => return Err(d.say(&name, step.print.kind)),
        }
        if self.cancelled() {
            return Ok(()); // the caller sees the cancel and changes nothing
        }
        if let Some(back) = &step.back {
            if back.symlink_metadata().is_ok() && !noclobber::same_item(&step.at, back) {
                return Err(format!("“{}” is already taken, so {name} can't go back", back.display()));
            }
            if let Some(parent) = back.parent().filter(|p| !p.is_dir()) {
                return Err(format!("the folder it came from, “{}”, is gone", parent.display()));
            }
        }
        Ok(())
    }

    /// For a step an earlier try left split across disks: the copy at `back` and the rest at
    /// `at` must both be as that try left them (the rest may have been removed since).
    pub(super) fn check_split(&self, name: &str, step: &Step, split: &Split, back: &Path) -> Result<(), String> {
        let (here, there) = (parent_of(&step.at), parent_of(back));
        if self.diff(back, &split.copy).is_some() {
            return Err(format!(
                "{name} was copied back to “{there}”, but that copy changed since, and part of the original is still in “{here}”"
            ));
        }
        match self.diff(&step.at, &split.rest) {
            None | Some(Diff::Gone) => Ok(()),
            Some(Diff::Unreachable(e)) => Err(format!(
                "{name} is back in “{there}”, but what's left of the original in “{here}” can't be checked ({e}), so it was left alone. Undo again once it can be read"
            )),
            Some(_) => {
                Err(format!("{name} is back in “{there}”, but what's left of the original in “{here}” changed since, so it was left alone"))
            }
        }
    }

    /// How `path` differs from `was` (including everything inside when `was` has a tree).
    pub(super) fn diff(&self, path: &Path, was: &Print) -> Option<Diff> {
        let m = match path.symlink_metadata() {
            Ok(m) => m,
            Err(e) if is_gone(&e) => return Some(Diff::Gone),
            Err(e) => return Some(Diff::Unreachable(e)),
        };
        let now = Print::of(path, &m);
        if now.kind != was.kind {
            return Some(Diff::Kind(now.kind));
        }
        if now.id != was.id {
            return Some(Diff::Replaced);
        }
        if now.size != was.size || now.mtime != was.mtime {
            return Some(Diff::Edited);
        }
        let tree = was.tree.as_ref()?;
        let mut seen = Tree::default();
        let read = self.walk(path, Path::new(""), tree, &mut seen);
        if self.cancelled() {
            return None; // the caller sees the cancel and changes nothing
        }
        match read {
            Err(_) => Some(Diff::Unreadable),
            Ok(()) if seen != *tree => Some(Diff::Inside(seen.items.cmp(&tree.items))),
            Ok(()) => None,
        }
    }

    /// Adds everything under `dir` to `seen`, stopping once it has seen more than `cap` has.
    pub(super) fn walk(&self, dir: &Path, rel: &Path, cap: &Tree, seen: &mut Tree) -> io::Result<()> {
        for de in std::fs::read_dir(dir)? {
            if self.cancelled() || seen.items > cap.items {
                return Ok(());
            }
            let de = de?;
            let path = de.path();
            let m = path.symlink_metadata()?;
            let rel = rel.join(de.file_name());
            seen.add(&rel, &Print::of(&path, &m));
            if m.is_dir() {
                self.walk(&path, &rel, cap, seen)?;
            }
        }
        Ok(())
    }

    /// `path`'s fingerprint, with everything inside when it's a folder.
    pub(super) fn print_all(&self, path: &Path) -> io::Result<Print> {
        let mut p = Print::of(path, &path.symlink_metadata()?);
        if p.kind == Kind::Folder {
            let mut t = Tree::default();
            self.walk(path, Path::new(""), &Tree::ALL, &mut t)?;
            p.tree = Some(t);
        }
        Ok(p)
    }
}
