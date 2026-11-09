//! Batch rename on the worker: every item checked first, swaps and chains through temporary
//! names, each step re-checked, and a rollback that never puts anything over anything.

use super::*;

// ---- batch rename ---------------------------------------------------------------------------

/// Where one item of a batch is while it's renamed, in the order it got there.
pub(super) struct Done {
    pub(super) item: usize,
    pub(super) now: PathBuf,
    pub(super) was: PathBuf,
}

impl Worker<'_> {
    /// A batch rename. Before touching anything, every item must still be as the preview saw it
    /// and every new name free (or held by another item of the batch, which moves out of the
    /// way); otherwise nothing is renamed and it says which item and why. Then
    /// [`Worker::rename_all`]: all of it, or none of it.
    pub(super) fn batch_rename(&mut self, items: Vec<Renaming>, out: &mut Outcome) {
        let moves: Vec<(PathBuf, PathBuf)> = items.iter().map(|r| (r.from.clone(), r.to.clone())).collect();
        let seen: Vec<&Print> = items.iter().map(|r| &r.seen.0).collect();
        if let Err(why) = self.check_batch(&moves, &seen) {
            out.refused = Some(format!("Nothing was renamed: {why}."));
            return;
        }
        if self.rename_all(&moves, &seen, out) {
            out.results = moves.iter().map(|(_, to)| to.clone()).collect();
            out.undo =
                Undo::record_or_say(What::BatchRename, moves.iter().map(|(from, to)| (to.clone(), Some(from.clone()))), &mut out.errors);
            out.moved = moves;
        }
    }

    /// Undoes a batch rename as one: every item must be as the batch left it and its old name
    /// free or held by another item of the batch, or nothing is undone. Then the same two-step
    /// plan, which swaps and chains need going back too. A failed or cancelled undo stays on
    /// the stack.
    pub(super) fn undo_batch_rename(&mut self, undo: Undo, out: &mut Outcome) {
        let moves: Vec<(PathBuf, PathBuf)> = undo.steps.iter().filter_map(|s| Some((s.at.clone(), s.back.clone()?))).collect();
        let seen: Vec<&Print> = undo.steps.iter().map(|s| &s.print).collect();
        if let Err(why) = self.check_batch(&moves, &seen) {
            out.refused = Some(format!("Can't {} the batch rename: {why}. Nothing was changed.", undo.word()));
            out.undo = Some(undo);
            return;
        }
        if self.rename_all(&moves, &seen, out) {
            out.results = moves.iter().map(|(_, back)| back.clone()).collect();
            // Redo (or undo again) goes through the same plan, the other way.
            let steps: Vec<Step> = moves.iter().filter_map(|(at, back)| self.inverse_step(back.clone(), Some(at.clone()), false)).collect();
            if !steps.is_empty() {
                out.inverse = Some(Undo { what: What::BatchRename, redo: !undo.redo, steps });
            }
            out.moved = moves;
        } else {
            out.undo = Some(undo);
        }
    }

    /// Whether every `(from, to)` can go: `from` still has fingerprint `seen`, and `to` is in
    /// the same folder and free, or is an item of the batch (which moves first). Says the first
    /// problem.
    pub(super) fn check_batch(&self, moves: &[(PathBuf, PathBuf)], seen: &[&Print]) -> Result<(), String> {
        for ((from, _), was) in moves.iter().zip(seen) {
            if let Some(d) = self.diff(from, was) {
                let name = format!("“{}”", name_of(from));
                return Err(match d {
                    Diff::Unreachable(e) => format!("{name} can't be checked ({e})"),
                    d => format!("{} since the preview", d.say(&name, was.kind)),
                });
            }
        }
        // An item inside another one being renamed: the folder's rename would move it from under
        // its own step, and undo couldn't find it (the preview says so first).
        for (inner, _) in moves {
            if let Some((outer, _)) = moves.iter().find(|(o, _)| o != inner && inner.starts_with(o)) {
                return Err(format!(
                    "“{}” is inside “{}”, which is being renamed too; rename them separately",
                    name_of(inner),
                    name_of(outer)
                ));
            }
        }
        let ids: std::collections::HashSet<(u64, u64)> = seen.iter().filter_map(|p| p.id).collect();
        let froms: std::collections::HashSet<String> = moves.iter().map(|(f, _)| fold_path(f)).collect();
        for (from, to) in moves {
            if from.parent() != to.parent() || to.file_name().is_none() {
                return Err(format!("“{}” would leave its folder", name_of(from)));
            }
            let Ok(m) = to.symlink_metadata() else { continue };
            let ours = match noclobber::file_id(to, &m) {
                Some(id) => ids.contains(&id),
                // No identity to tell by: only a name the batch itself holds.
                None => froms.contains(&fold_path(to)),
            };
            if !ours {
                return Err(format!("“{}” already exists in “{}”", name_of(to), parent_of(to)));
            }
        }
        Ok(())
    }

    /// Renames each `(from, to)` (each in its own folder), never over anything, as one step
    /// that either all happens or is put back. An item whose new name is another item's old one
    /// (a swap, a chain, a case-only change) first steps aside to a temporary name beside it
    /// (`.arbor-…`, as [`noclobber::rename`]'s case-only renames do); then the others get
    /// their new names, freeing the old ones; then the ones aside get theirs. A failure or a
    /// cancel midway puts every item back, last step first, again never over anything; what
    /// can't go back (another app took its old name meanwhile, or changed the item since it was
    /// renamed) stays where it is and the message says exactly where. Each item is checked
    /// against `seen` (its fingerprint when the plan was made) right before each of its steps
    /// and before it's put back, so an item replaced or edited while the batch runs is never
    /// renamed, and a replacement is never put back in an item's place. True when every item
    /// has its new name.
    pub(super) fn rename_all(&mut self, moves: &[(PathBuf, PathBuf)], seen: &[&Print], out: &mut Outcome) -> bool {
        self.p.items_total.store(moves.len(), Relaxed);
        let froms: std::collections::HashSet<String> = moves.iter().map(|(f, _)| fold_path(f)).collect();
        // Compared case aside on every disk: a needless step aside costs one rename.
        let aside: Vec<bool> = moves.iter().map(|(_, to)| froms.contains(&fold_path(to))).collect();
        let mut done: Vec<Done> = Vec::new();
        let Err(why) = self.rename_steps(moves, seen, &aside, &mut done) else { return true };
        let stuck = self.put_back(done, seen);
        let cancelled = self.cancelled();
        let lead = if cancelled { "Cancelled".to_string() } else { why };
        if stuck.is_empty() {
            out.errors.push(format!("{lead}. Nothing was renamed: every item has its old name again"));
            return false;
        }
        let at = |(item, now, why): &(usize, PathBuf, Stuck)| match why {
            Stuck::Taken => format!(
                "“{}” is now “{}” in “{}” (something else took its old name)",
                name_of(&moves[*item].0),
                name_of(now),
                parent_of(now)
            ),
            Stuck::Changed => format!(
                "“{}” in “{}” changed after it was renamed (it was “{}”), so it was left there",
                name_of(now),
                parent_of(now),
                name_of(&moves[*item].0)
            ),
        };
        let shown: Vec<String> = stuck.iter().take(3).map(at).collect();
        let more = if stuck.len() > 3 { format!(" (and {} more)", stuck.len() - 3) } else { String::new() };
        let back = moves.len() - stuck.len();
        out.errors.push(format!("{lead}. {back} of {} items have their old names again; {}{more}", moves.len(), shown.join("; ")));
        out.errors.extend(stuck.iter().skip(3).map(at));
        false
    }

    /// Whether the item at `at` (first at `orig`) is still the one the plan was made for
    /// (fingerprint `was`); says how it changed when not.
    pub(super) fn step_check(&self, at: &Path, orig: &Path, was: &Print) -> Result<(), String> {
        match self.diff(at, was) {
            None => Ok(()),
            Some(Diff::Unreachable(e)) => Err(format!("“{}” can't be checked ({e})", name_of(orig))),
            Some(d) => Err(format!("{} while the batch was renaming", d.say(&format!("“{}”", name_of(orig)), was.kind))),
        }
    }

    /// The steps of [`Worker::rename_all`], recording each in `done`. The first failure (or a
    /// cancel) stops it and says why.
    pub(super) fn rename_steps(
        &mut self,
        moves: &[(PathBuf, PathBuf)],
        seen: &[&Print],
        aside: &[bool],
        done: &mut Vec<Done>,
    ) -> Result<(), String> {
        let mut at: Vec<PathBuf> = moves.iter().map(|(from, _)| from.clone()).collect();
        for (i, (from, _)) in moves.iter().enumerate().filter(|&(i, _)| aside[i]) {
            if self.cancelled() {
                return Err("Cancelled".into());
            }
            self.step_check(from, from, seen[i])?;
            let tmp = self.step_aside(from)?;
            done.push(Done { item: i, now: tmp.clone(), was: from.clone() });
            at[i] = tmp;
            self.tick(0);
        }
        for pass in [false, true] {
            for (i, (from, to)) in moves.iter().enumerate().filter(|&(i, _)| aside[i] == pass) {
                if self.cancelled() {
                    return Err("Cancelled".into());
                }
                self.before_act(to);
                // Right before its own step: still the item the plan was made for.
                self.step_check(&at[i], from, seen[i])?;
                match self.rename_excl(&at[i], to) {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                        return Err(format!("“{}” was taken just now, so “{}” couldn't have that name", name_of(to), name_of(from)));
                    }
                    Err(e) => return Err(format!("Couldn't rename “{}” to “{}”: {e}", name_of(from), name_of(to))),
                }
                done.push(Done { item: i, now: to.clone(), was: std::mem::replace(&mut at[i], to.clone()) });
                self.p.items_done.fetch_add(1, Relaxed);
                self.tick(0);
            }
        }
        Ok(())
    }

    /// Moves `from` to a free temporary name beside it; returns the name.
    pub(super) fn step_aside(&mut self, from: &Path) -> Result<PathBuf, String> {
        for _ in 0..TRIES {
            let tmp = noclobber::temp_sibling(from).map_err(|e| e.to_string())?;
            match self.rename_excl(from, &tmp) {
                Ok(()) => return Ok(tmp),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("Couldn't rename “{}”: {e}", name_of(from))),
            }
        }
        Err(format!("Couldn't find a free temporary name beside “{}”", name_of(from)))
    }

    /// Undoes `done`, last step first, never over anything, and only while what's at each
    /// step's new name is still the item that went there (`seen`): a replacement or an edited
    /// item is never moved into an item's old name. An item that can't go back stays where it is
    /// (its earlier steps aren't tried). Returns those, with where each is and why.
    pub(super) fn put_back(&mut self, done: Vec<Done>, seen: &[&Print]) -> Vec<(usize, PathBuf, Stuck)> {
        let mut stuck: Vec<(usize, PathBuf, Stuck)> = Vec::new();
        for d in done.into_iter().rev() {
            if stuck.iter().any(|(i, _, _)| *i == d.item) {
                continue;
            }
            self.before_act(&d.was);
            if self.diff(&d.now, seen[d.item]).is_some() {
                stuck.push((d.item, d.now, Stuck::Changed));
            } else if self.rename_excl(&d.now, &d.was).is_err() {
                stuck.push((d.item, d.now, Stuck::Taken));
            }
        }
        stuck.sort_by_key(|(i, _, _)| *i);
        stuck
    }
}

/// Why a batch rename's rollback left an item where it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stuck {
    /// Something else took its old name.
    Taken,
    /// What's at its new name isn't the item that went there any more (replaced or edited).
    Changed,
}
