//! Batch Rename's dialog (NORTHSTAR Core: "regex, numbering, preview before apply"). With two
//! or more items selected, Rename (F2, ⇧⌘R) opens it instead of renaming inline, as Finder
//! does; "Batch Rename…" in the palette opens it for any selection. It works on search results
//! too: each item is renamed in its own folder.
//!
//! The rule's fields are in `rename.rs` (pure functions on names). The dialog:
//!
//! - **Reads each folder once, on a worker**, when it opens: what else is in it (a new name
//!   already taken there is a problem), whether its disk ignores case (by looking up one name
//!   with its case swapped), and every item's fingerprint, so applying refuses an item that
//!   changed since. The window waits at most a frame for it; until it lands, Apply waits.
//! - **Previews every keystroke.** A batch of up to [`SYNC_ITEMS`] is planned on the spot
//!   (under a millisecond); a bigger one on a worker, showing the last plan until the new one
//!   lands (10k names through a regex take about 20 ms, more than a frame). Only the rows in
//!   view are built.
//! - **Applies as one file operation** (`ops::Op::Rename`) on the operations worker, with
//!   progress, cancellable, all or nothing, and one undo entry. Apply is disabled while
//!   anything has a problem; Enter applies (every field is one line), Esc closes. The rule is
//!   kept for the session, so the dialog opens with it next time.

use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use rune::{Key, KeyEvent, Latest, TextEdit, Tx};

use crate::app::{BUSY, State};
use crate::ops::{Op, Renaming, Seen};
use crate::rename::{self, CaseMode, Dir, Item, Plan, Platform, Rule, Status};

/// Batches up to this many items are planned on the UI thread, on each key (regex at about
/// 2 µs a name: well under a millisecond); bigger ones on a worker.
pub const SYNC_ITEMS: usize = 500;

/// How long a key waits for a bigger batch's plan from the worker before drawing the last one:
/// cheap rules over thousands of names (plain text: about 0.3 µs a name) land in the key's own
/// frame, and a slow one (a regex over 10k names, about 20 ms) lands when it's done.
const PLAN_WAIT: std::time::Duration = std::time::Duration::from_millis(3);

/// The platform whose rules names are checked against.
const PLATFORM: Platform = Platform::current();

/// The dialog's text fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Find,
    Replace,
    Start,
    Step,
}

/// What the worker read when the dialog opened.
pub struct Read {
    dirs: Vec<Dir>,
    seen: Vec<Option<Seen>>,
}

pub struct BatchBox {
    pub find: TextEdit,
    pub replace: TextEdit,
    pub start: TextEdit,
    pub step: TextEdit,
    pub regex: bool,
    pub case_sensitive: bool,
    pub case: CaseMode,
    pub include_ext: bool,
    /// The items in the order they were listed, and where each is.
    pub items: Arc<Vec<Item>>,
    pub paths: Vec<PathBuf>,
    /// The folders they're in, and what's known about each.
    pub dirs: Arc<Vec<Dir>>,
    /// Each item's fingerprint, once the folders have been read.
    seen: Option<Vec<Option<Seen>>>,
    /// The plan on screen, the rule it's for (and whether the folders were known then), and
    /// the rule a worker is planning for.
    pub plan: Plan,
    planned_for: Option<(Rule, bool)>,
    pending: Option<(Rule, bool)>,
    /// Why the rule can't be used (a bad regex, a bad number).
    pub error: Option<String>,
    /// Enter was pressed while a plan was being worked out: apply when it lands.
    apply_waiting: bool,
    round: u64,
    reader: Latest<Read>,
    stop_read: Arc<AtomicBool>,
    planner: Latest<(u64, Plan)>,
}

impl BatchBox {
    /// The rule as the fields hold it.
    pub fn rule(&self) -> Rule {
        Rule {
            find: self.find.text.clone(),
            regex: self.regex,
            case_sensitive: self.case_sensitive,
            replace: self.replace.text.clone(),
            start: self.start.text.clone(),
            step: self.step.text.clone(),
            case: self.case,
            include_ext: self.include_ext,
        }
    }

    /// Whether the folders have been read.
    pub fn known(&self) -> bool {
        self.seen.is_some()
    }

    /// Why Apply can't run now, or `None` when it can.
    pub fn blocked(&self) -> Option<String> {
        if let Some(e) = &self.error {
            return Some(e.clone());
        }
        if !self.known() {
            return Some("Checking the folders…".into());
        }
        if self.planned_for.as_ref() != Some(&(self.rule(), true)) {
            return Some("Updating…".into());
        }
        if self.plan.problems > 0 {
            return Some("Fix the problems to rename".into());
        }
        if self.plan.changes == 0 {
            return Some("Nothing will change".into());
        }
        None
    }

    /// The plan on screen is for an older rule (a worker is on the new one).
    pub fn stale(&self) -> bool {
        self.pending.is_some()
    }
}

impl State {
    /// Opens the dialog on the active pane's selection, with the rule used last.
    pub fn open_batch(&mut self, tx: &mut Tx) {
        let f = self.cur();
        let targets = f.targets();
        if targets.is_empty() {
            return;
        }
        // Items in order, each with its folder (in a search, results come from many; items of
        // one folder mostly come together, so the last folder is checked first).
        let mut paths = Vec::with_capacity(targets.len());
        let mut items = Vec::with_capacity(targets.len());
        let mut dir_paths: Vec<PathBuf> = Vec::new();
        let mut dir_ix: HashMap<PathBuf, u32> = HashMap::new();
        let keep =
            |os: &std::ffi::OsStr| os.to_str().is_none().then_some("Its name isn't valid Unicode, so it keeps it (rename it on its own)");
        if !f.searching() {
            // A folder: one folder, and each entry's name is its own.
            dir_paths.push(f.cwd.clone());
            for e in &targets {
                items.push(Item { name: e.name.clone(), is_dir: e.is_dir, dir: 0, keep: keep(&e.os), gone: false });
                paths.push(f.cwd.join(&e.os));
            }
        }
        for e in targets.iter().filter(|_| f.searching()) {
            let p = f.cwd.join(&e.os);
            let parent = p.parent().unwrap_or(&p);
            let dir = match dir_paths.last() {
                Some(last) if last == parent => (dir_paths.len() - 1) as u32,
                _ => *dir_ix.entry(parent.to_path_buf()).or_insert_with(|| {
                    dir_paths.push(parent.to_path_buf());
                    (dir_paths.len() - 1) as u32
                }),
            };
            let os = p.file_name().unwrap_or_default();
            items.push(Item { name: e.name.clone(), is_dir: e.is_dir, dir, keep: keep(os), gone: false });
            paths.push(p);
        }
        let dirs: Vec<Dir> =
            dir_paths.iter().map(|d| Dir { name: crate::app::title(d), ignores_case: PLATFORM.ignores_case(), ..Dir::default() }).collect();
        self.close_overlays(tx);
        if self.rename.take().is_some() {
            tx.blur();
        }
        let ix = self.active;
        self.focus_results(ix, tx);
        let r = self.batch_rule.clone();
        let mut find = TextEdit::new(r.find.clone());
        find.set_selection(0, r.find.len());
        find.focused = true;
        let reader: Latest<Read> = Latest::default();
        let planner: Latest<(u64, Plan)> = Latest::default();
        if let Some(w) = &self.waker {
            reader.set_waker(w.clone());
            planner.set_waker(w.clone());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let items = Arc::new(items);
        let mut b = BatchBox {
            find,
            replace: TextEdit::new(r.replace.clone()),
            start: TextEdit::new(r.start.clone()),
            step: TextEdit::new(r.step.clone()),
            regex: r.regex,
            case_sensitive: r.case_sensitive,
            case: r.case,
            include_ext: r.include_ext,
            items: items.clone(),
            paths: paths.clone(),
            dirs: Arc::new(dirs),
            seen: None,
            plan: Plan::default(),
            planned_for: None,
            pending: None,
            error: None,
            apply_waiting: false,
            round: 0,
            reader,
            stop_read: stop.clone(),
            planner,
        };
        // A few items' folders are read in time for this frame; for many (one lstat each),
        // the read can't be, so the dialog doesn't wait for it.
        let wait = if items.len() <= SYNC_ITEMS { self.show_wait } else { std::time::Duration::ZERO };
        let names: Vec<String> = b.dirs.iter().map(|d| d.name.clone()).collect();
        let read = crate::folder::read_waiting(&b.reader, wait, move || read_folders(&dir_paths, &names, &items, &paths, &stop));
        if let Some(r) = read {
            land_read(&mut b, r);
        }
        self.batch = Some(b);
        // Opening doesn't wait for a big batch's plan: its rows show their names until it lands.
        self.batch_replan_within(std::time::Duration::ZERO);
    }

    /// Closes the dialog, keeping its rule for next time.
    pub fn close_batch(&mut self, tx: &mut Tx) {
        if let Some(b) = self.batch.take() {
            b.stop_read.store(true, Relaxed);
            self.batch_rule = b.rule();
            tx.blur();
        }
    }

    /// A field's binding (late writes after it closes go to a scratch field).
    pub fn batch_edit(&mut self, field: Field) -> &mut TextEdit {
        match &mut self.batch {
            Some(b) => match field {
                Field::Find => &mut b.find,
                Field::Replace => &mut b.replace,
                Field::Start => &mut b.start,
                Field::Step => &mut b.step,
            },
            None => &mut self.palette_sink,
        }
    }

    /// Plans again if the rule or what's known changed since. Small batches here and now;
    /// bigger ones on a worker, waiting [`PLAN_WAIT`] for it (the plan on screen stays until
    /// the new one lands).
    pub fn batch_replan(&mut self) {
        self.batch_replan_within(PLAN_WAIT);
    }

    fn batch_replan_within(&mut self, wait: std::time::Duration) {
        let Some(b) = &mut self.batch else { return };
        let want = (b.rule(), b.known());
        if b.pending.as_ref() == Some(&want) || (b.pending.is_none() && b.planned_for.as_ref() == Some(&want)) {
            return;
        }
        b.round += 1;
        match want.0.compile() {
            Err(e) => {
                b.error = Some(e);
                b.pending = None;
                b.planned_for = None;
            }
            Ok(c) => {
                b.error = None;
                let plan = if b.items.len() <= SYNC_ITEMS {
                    Some(rename::plan(&c, &b.items, &b.dirs, PLATFORM))
                } else {
                    let (items, dirs, round) = (b.items.clone(), b.dirs.clone(), b.round);
                    let work = move || Some((round, rename::plan(&c, &items, &dirs, PLATFORM)));
                    crate::folder::read_waiting(&b.planner, wait, work).map(|(_, plan)| plan)
                };
                match plan {
                    Some(plan) => {
                        b.plan = plan;
                        b.planned_for = Some(want);
                        b.pending = None;
                        self.batch_plans += 1;
                    }
                    None => b.pending = Some(want),
                }
            }
        }
    }

    /// From `poll`: the folders' read and plans from the worker. True when the view changes.
    pub fn poll_batch(&mut self) -> bool {
        let Some(b) = &mut self.batch else { return false };
        let mut changed = false;
        if let Some(r) = b.reader.take() {
            land_read(b, r);
            changed = true;
        }
        if let Some((round, plan)) = b.planner.take()
            && round == b.round
        {
            b.plan = plan;
            b.planned_for = b.pending.take();
            self.batch_plans += 1;
            changed = true;
        }
        if changed {
            self.batch_replan();
            if self.batch.as_ref().is_some_and(|b| b.apply_waiting && !b.stale()) {
                self.batch_apply(&mut Tx::default());
            }
        }
        changed
    }

    /// A toggle or the case changed: plan again.
    pub fn batch_set(&mut self, f: impl FnOnce(&mut BatchBox)) {
        if let Some(b) = &mut self.batch {
            f(b);
        }
        self.batch_replan();
    }

    /// Enter or the Rename button: one file operation for every item that changes. While the
    /// plan for the current rule is still on a worker, it applies when that lands.
    pub fn batch_apply(&mut self, tx: &mut Tx) {
        let Some(b) = &mut self.batch else { return };
        if b.stale() {
            b.apply_waiting = true;
            return;
        }
        b.apply_waiting = false;
        if let Some(why) = b.blocked() {
            self.note = Some(why);
            return;
        }
        if self.job.is_some() {
            self.note = Some(BUSY.into());
            return;
        }
        let Some(seen) = &b.seen else { return };
        let items: Vec<Renaming> = (0..b.items.len())
            .filter(|&i| b.plan.status[i] == Status::Change)
            .filter_map(|i| {
                let from = b.paths[i].clone();
                Some(Renaming { to: from.with_file_name(&b.plan.new[i]), from, seen: seen[i].clone()? })
            })
            .collect();
        self.close_batch(tx);
        self.start(Op::Rename { items });
    }

    /// The fields' keys, before they edit: Enter applies (every field is one line), Esc
    /// closes, ⌥R, ⌥C and ⌥E flip Regex, Match Case and Include Extension.
    pub fn batch_field_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        tx.instant();
        let m = ev.mods;
        match ev.key {
            Key::Enter if !m.cmd && !m.ctrl && !m.alt && !m.shift => self.batch_apply(tx),
            Key::Escape if !m.cmd && !m.ctrl && !m.alt => self.close_batch(tx),
            _ => {
                let chord = |spec: &str| crate::commands::parse_chord(spec).is_ok_and(|c| c.matches_tx(ev, tx));
                if chord("alt+r") {
                    self.batch_set(|b| b.regex = !b.regex);
                } else if chord("alt+c") {
                    self.batch_set(|b| b.case_sensitive = !b.case_sensitive);
                } else if chord("alt+e") {
                    self.batch_set(|b| b.include_ext = !b.include_ext);
                } else {
                    return false;
                }
            }
        }
        true
    }

    /// The window's keys while the dialog is open (no field has the focus): the same keys as
    /// in a field, typing goes to the field focused last (Find at first), Tab goes on to focus
    /// a field, and nothing else reaches the window.
    pub fn batch_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        if ev.key == Key::Tab {
            return false;
        }
        if self.batch_field_key(ev, tx) {
            return true;
        }
        if let (Key::Text(t), Some(b)) = (&ev.key, &mut self.batch)
            && !ev.mods.cmd
            && !ev.mods.ctrl
        {
            let e = match [&b.replace, &b.start, &b.step].iter().position(|e| e.focused) {
                Some(0) => &mut b.replace,
                Some(1) => &mut b.start,
                Some(2) => &mut b.step,
                _ => &mut b.find,
            };
            e.insert(t);
            e.focused = true;
            self.batch_replan();
        }
        true
    }
}

/// Applies what the worker read: each folder's other names and its case rule, and which items
/// are gone.
fn land_read(b: &mut BatchBox, r: Read) {
    if r.seen.iter().any(Option::is_none) {
        let items: Vec<Item> = b.items.iter().zip(&r.seen).map(|(it, s)| Item { gone: s.is_none(), ..it.clone() }).collect();
        b.items = Arc::new(items);
    }
    b.dirs = Arc::new(r.dirs);
    b.seen = Some(r.seen);
}

/// On a worker: each folder's names besides the batch's `items`, whether its disk ignores
/// case, and each item's fingerprint. `None` when stopped (the dialog closed).
fn read_folders(dir_paths: &[PathBuf], names: &[String], items: &[Item], paths: &[PathBuf], stop: &AtomicBool) -> Option<Read> {
    let mut batch: Vec<HashSet<&str>> = vec![HashSet::new(); dir_paths.len()];
    for it in items {
        batch[it.dir as usize].insert(it.name.as_str());
    }
    let mut dirs = Vec::with_capacity(dir_paths.len());
    for ((d, name), batch) in dir_paths.iter().zip(names).zip(&batch) {
        let mut all: Vec<OsString> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                if stop.load(Relaxed) {
                    return None;
                }
                all.push(e.file_name());
            }
        }
        let ignores_case = ignores_case(d, &all).unwrap_or(PLATFORM.ignores_case());
        let others = all
            .iter()
            .map(|n| n.to_string_lossy())
            .filter(|n| !batch.contains(n.as_ref()))
            .map(|n| if ignores_case { n.to_lowercase() } else { n.into_owned() })
            .collect();
        dirs.push(Dir { name: name.clone(), others, ignores_case, known: true });
    }
    let mut seen = Vec::with_capacity(paths.len());
    for p in paths {
        if stop.load(Relaxed) {
            return None;
        }
        seen.push(Seen::of(p));
    }
    Some(Read { dirs, seen })
}

/// Whether the disk holding folder `d` ignores case: one of its names with its case swapped is
/// looked up, and finds the same item. `None` when no name has letters to swap.
fn ignores_case(d: &std::path::Path, names: &[OsString]) -> Option<bool> {
    for n in names {
        let Some(s) = n.to_str() else { continue };
        let swapped: String = s
            .chars()
            .map(|c| if c.is_lowercase() { c.to_uppercase().next().unwrap_or(c) } else { c.to_lowercase().next().unwrap_or(c) })
            .collect();
        if swapped == s {
            continue;
        }
        // The swapped spelling is itself another item (on a disk that tells case apart): try
        // the next name.
        if names.iter().any(|m| m.to_str() == Some(swapped.as_str())) {
            continue;
        }
        return Some(crate::noclobber::same_item(&d.join(s), &d.join(&swapped)));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{settle_all, state_in};
    use crate::commands::Cmd;
    use std::time::{Duration, Instant};

    /// Polls as frames would until the dialog's read and plan have landed (or 10 s pass).
    pub(crate) fn settle_batch(s: &mut State) {
        let until = Instant::now() + Duration::from_secs(10);
        while s.batch.as_ref().is_some_and(|b| !b.known() || b.stale()) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
            s.poll(&mut Tx::default());
        }
        assert!(s.batch.as_ref().is_none_or(|b| b.known() && !b.stale()), "never landed");
    }

    fn folder(name: &str, files: &[&str]) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-batch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for f in files {
            std::fs::write(d.join(f), format!("I was {f}")).unwrap();
        }
        d
    }

    fn names(d: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(d).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    fn key(k: Key) -> KeyEvent {
        KeyEvent { key: k, mods: rune::Mods::default() }
    }

    /// Selects the rows named `names` in the active pane.
    fn select(s: &mut State, names: &[&str]) {
        let set: HashSet<OsString> = names.iter().map(OsString::from).collect();
        s.cur_mut().set_selection(set, &HashSet::new());
    }

    /// Polls until the file operation is done and every pane has settled.
    fn finish_job(s: &mut State) {
        let until = Instant::now() + Duration::from_secs(10);
        while s.job.is_some() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
            s.poll(&mut Tx::default());
        }
        assert!(s.job.is_none(), "the operation never finished");
        settle_all(s);
    }

    fn set(s: &mut State, f: impl FnOnce(&mut BatchBox)) {
        s.batch_set(f);
        settle_batch(s);
    }

    fn marked(s: &State) -> Vec<String> {
        let mut v: Vec<String> = s.cur().marked.iter().map(|n| n.to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    /// With two or more selected, Rename (F2) opens Batch Rename, as Finder does; with one, it
    /// renames inline. "Batch Rename…" opens it for any selection.
    #[test]
    fn rename_with_several_selected_opens_batch_rename() {
        let d = folder("open", &["a.txt", "b.txt"]);
        let mut s = state_in(&d);
        let mut tx = Tx::default();
        s.run(Cmd::Rename, &mut tx);
        assert!(s.rename.is_some() && s.batch.is_none(), "one item: inline");
        s.on_key(&key(Key::Escape), &mut tx);
        select(&mut s, &["a.txt", "b.txt"]);
        assert_eq!(s.title(Cmd::Rename), "Batch Rename…");
        s.on_key(&key(Key::Function(2)), &mut tx);
        assert!(s.rename.is_none());
        let b = s.batch.as_ref().expect("F2 opens Batch Rename");
        assert_eq!(b.items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["a.txt", "b.txt"]);
        assert!(b.known(), "a small folder is read within the frame");
        // Keys don't leak to the window; Esc closes it and keeps the rule.
        s.on_key(&key(Key::Text("x".into())), &mut tx);
        assert_eq!(s.batch.as_ref().unwrap().find.text, "x", "typing goes to Find");
        assert_eq!(s.cur().filter, "");
        s.on_key(&key(Key::Escape), &mut tx);
        assert!(s.batch.is_none());
        assert_eq!(s.batch_rule.find, "x");
        // From the palette, with one item.
        s.cur_mut().select(0);
        assert_eq!(s.unavailable(Cmd::BatchRename), None);
        s.run(Cmd::BatchRename, &mut tx);
        assert_eq!(s.batch.as_ref().unwrap().find.text, "x", "opens with the last rule");
        assert_eq!(s.batch.as_ref().unwrap().items.len(), 1);
    }

    /// Regex and numbering, applied: one operation, the renamed items selected under their new
    /// names, and one undo entry that puts them all back.
    #[test]
    fn apply_renames_selects_and_undoes_as_one() {
        let d = folder("apply", &["IMG_1.jpg", "IMG_2.jpg", "notes.txt"]);
        let mut s = state_in(&d);
        let mut tx = Tx::default();
        select(&mut s, &["IMG_1.jpg", "IMG_2.jpg", "notes.txt"]);
        s.run(Cmd::Rename, &mut tx);
        set(&mut s, |b| {
            b.regex = true;
            b.find = TextEdit::new(r"^IMG_(\d)$");
            b.replace = TextEdit::new("Trip {n:2} ($1)");
            b.start = TextEdit::new("5");
        });
        let b = s.batch.as_ref().unwrap();
        assert_eq!(b.plan.new, ["Trip 05 (1).jpg", "Trip 06 (2).jpg", "notes.txt"], "notes.txt isn't counted");
        assert_eq!(b.plan.summary(), "2 will change");
        assert_eq!(b.blocked(), None);
        let undo = s.undo.len();
        assert!(s.batch_field_key(&key(Key::Enter), &mut tx));
        assert!(s.batch.is_none() && s.job.is_some(), "one operation on the worker");
        finish_job(&mut s);
        assert_eq!(names(&d), ["Trip 05 (1).jpg", "Trip 06 (2).jpg", "notes.txt"]);
        assert_eq!(marked(&s), ["Trip 05 (1).jpg", "Trip 06 (2).jpg"], "the renamed items, selected");
        assert_eq!(s.note.as_deref(), Some("Renamed 2 items"));
        assert_eq!(s.undo.len(), undo + 1, "one undo entry");
        s.run(Cmd::Undo, &mut tx);
        finish_job(&mut s);
        assert_eq!(names(&d), ["IMG_1.jpg", "IMG_2.jpg", "notes.txt"]);
        assert_eq!(marked(&s), ["IMG_1.jpg", "IMG_2.jpg"]);
        assert_eq!(std::fs::read_to_string(d.join("IMG_2.jpg")).unwrap(), "I was IMG_2.jpg");
    }

    /// A swap through the dialog: each name is the other's.
    #[test]
    fn a_swap_through_the_dialog() {
        let d = folder("swap", &["a", "b"]);
        let mut s = state_in(&d);
        let mut tx = Tx::default();
        select(&mut s, &["a", "b"]);
        s.run(Cmd::Rename, &mut tx);
        set(&mut s, |b| {
            b.regex = true;
            b.find = TextEdit::new("^.$");
            b.replace = TextEdit::new("{n}");
            b.start = TextEdit::new("1");
        });
        // a → 1, b → 2; then 1 → b, 2 → a by a second batch: a swap of the originals.
        s.batch_apply(&mut tx);
        finish_job(&mut s);
        select(&mut s, &["1", "2"]);
        s.run(Cmd::Rename, &mut tx);
        set(&mut s, |b| {
            b.regex = true;
            b.find = TextEdit::new("^1$");
            b.replace = TextEdit::new("2");
        });
        let b = s.batch.as_ref().unwrap();
        assert_eq!(b.plan.new, ["2", "2"]);
        assert!(b.blocked().is_some(), "1 → 2 while 2 stays: a duplicate");
        set(&mut s, |b| {
            b.find = TextEdit::new("^(1|2)$");
            b.replace = TextEdit::new("x$1");
        });
        s.batch_apply(&mut tx);
        finish_job(&mut s);
        assert_eq!(names(&d), ["x1", "x2"]);
        // And a real swap: x1 ↔ x2.
        select(&mut s, &["x1", "x2"]);
        s.run(Cmd::Rename, &mut tx);
        set(&mut s, |b| {
            b.find = TextEdit::new("^x(1|2)$");
            b.replace = TextEdit::new("x{n}");
            b.start = TextEdit::new("2");
            b.step = TextEdit::new("-1");
        });
        assert_eq!(s.batch.as_ref().unwrap().plan.new, ["x2", "x1"]);
        assert_eq!(s.batch.as_ref().unwrap().blocked(), None, "a swap isn't a problem");
        s.batch_apply(&mut tx);
        finish_job(&mut s);
        assert_eq!(std::fs::read_to_string(d.join("x1")).unwrap(), "I was b");
        assert_eq!(std::fs::read_to_string(d.join("x2")).unwrap(), "I was a");
    }

    /// Problems block Rename and say why; nothing starts.
    #[test]
    fn problems_block_rename() {
        let d = folder("blocked", &["a.txt", "b.txt", "taken.txt"]);
        let mut s = state_in(&d);
        let mut tx = Tx::default();
        select(&mut s, &["a.txt", "b.txt"]);
        s.run(Cmd::Rename, &mut tx);
        set(&mut s, |b| {
            b.find = TextEdit::new("a");
            b.replace = TextEdit::new("taken");
        });
        let b = s.batch.as_ref().unwrap();
        assert_eq!(b.plan.status[0], Status::Problem("“taken.txt” already exists here".into()));
        assert_eq!(b.blocked().as_deref(), Some("Fix the problems to rename"));
        s.batch_field_key(&key(Key::Enter), &mut tx);
        assert!(s.batch.is_some() && s.job.is_none(), "nothing started");
        assert_eq!(s.note.as_deref(), Some("Fix the problems to rename"));
        // A bad regex too.
        set(&mut s, |b| {
            b.regex = true;
            b.find = TextEdit::new("(");
        });
        assert!(s.batch.as_ref().unwrap().blocked().unwrap().starts_with("Regex: "));
        assert_eq!(names(&d), ["a.txt", "b.txt", "taken.txt"]);
    }

    /// An item edited after the preview read it: the whole batch is refused, saying which.
    #[test]
    fn an_item_changed_since_the_preview_is_refused() {
        let d = folder("changed", &["a.txt", "b.txt"]);
        let mut s = state_in(&d);
        let mut tx = Tx::default();
        select(&mut s, &["a.txt", "b.txt"]);
        s.run(Cmd::Rename, &mut tx);
        set(&mut s, |b| b.replace = TextEdit::new("new {n}"));
        let f = std::fs::File::options().write(true).open(d.join("b.txt")).unwrap();
        f.set_len(1).unwrap();
        f.set_modified(std::time::SystemTime::now() + Duration::from_secs(3600)).unwrap();
        s.batch_apply(&mut tx);
        finish_job(&mut s);
        assert_eq!(s.note.as_deref(), Some("Nothing was renamed: “b.txt” was edited since the preview."));
        assert_eq!(names(&d), ["a.txt", "b.txt"]);
    }

    /// A batch bigger than [`SYNC_ITEMS`] is planned on a worker; Enter before the plan lands
    /// applies once it has.
    #[test]
    fn a_big_batch_plans_on_a_worker_and_enter_waits_for_it() {
        let files: Vec<String> = (0..SYNC_ITEMS + 20).map(|i| format!("f{i:04}")).collect();
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        let d = folder("big", &refs);
        let mut s = state_in(&d);
        settle_all(&mut s);
        let mut tx = Tx::default();
        s.run(Cmd::SelectAll, &mut tx);
        s.run(Cmd::Rename, &mut tx);
        settle_batch(&mut s);
        let b = s.batch.as_mut().unwrap();
        b.find = TextEdit::new("f");
        b.replace = TextEdit::new("g");
        s.batch_replan_within(Duration::ZERO);
        assert!(s.batch.as_ref().unwrap().stale(), "on the worker");
        assert_eq!(s.batch.as_ref().unwrap().blocked().as_deref(), Some("Updating…"));
        s.batch_apply(&mut tx);
        assert!(s.batch.is_some() && s.job.is_none(), "waits for the plan");
        let until = Instant::now() + Duration::from_secs(10);
        while s.job.is_none() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
            s.poll(&mut tx);
        }
        assert!(s.batch.is_none(), "applied when it landed");
        finish_job(&mut s);
        assert!(names(&d).iter().all(|n| n.starts_with('g')), "{:?}", &names(&d)[..3]);
        assert_eq!(s.cur().marked.len(), SYNC_ITEMS + 20);
    }

    /// What opening the dialog on 10k items and typing in it cost on the UI thread:
    /// `cargo test --release -- --ignored --nocapture batch_speed`.
    #[test]
    #[ignore = "benchmark"]
    fn batch_speed() {
        let d = std::env::temp_dir().join(format!("arbor-batch-speed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for i in 0..10_000 {
            std::fs::write(d.join(format!("IMG_{i:05} holiday.JPG")), "").unwrap();
        }
        let mut s = state_in(&d);
        settle_all(&mut s);
        let mut tx = Tx::default();
        for round in 0..5 {
            s.run(Cmd::SelectAll, &mut tx);
            let t = Instant::now();
            s.run(Cmd::BatchRename, &mut tx);
            let open = t.elapsed();
            let t = Instant::now();
            settle_batch(&mut s);
            let read = t.elapsed();
            let key = |s: &mut State, k: &str| {
                let t = Instant::now();
                s.on_key(&KeyEvent { key: Key::Text(k.into()), mods: rune::Mods::default() }, &mut Tx::default());
                let handler = t.elapsed();
                let t = Instant::now();
                settle_batch(s);
                (handler, t.elapsed())
            };
            let plain = key(&mut s, "e");
            s.batch_set(|b| b.regex = true);
            settle_batch(&mut s);
            let regex = key(&mut s, "$");
            println!(
                "round {round}: open {:.2} ms (read lands {:.1} ms later); 'e' handler {:.2} ms, plan {:.1} ms; regex '$' handler {:.2} ms, plan {:.1} ms",
                open.as_secs_f64() * 1e3,
                read.as_secs_f64() * 1e3,
                plain.0.as_secs_f64() * 1e3,
                plain.1.as_secs_f64() * 1e3,
                regex.0.as_secs_f64() * 1e3,
                regex.1.as_secs_f64() * 1e3,
            );
            s.close_batch(&mut tx);
            s.batch_rule = Rule::default();
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
