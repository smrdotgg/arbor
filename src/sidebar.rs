//! The sidebar's favorites: the folders listed under "Favorites", in the user's order. It's
//! user data (NORTHSTAR: "User data is not a cache"), kept in `sidebar.toml` beside the settings
//! file, not in `config.toml`, which people edit by hand.
//!
//! - **No file:** the defaults, as before: the settings' `[general] places`, or the folders
//!   found on this machine (`fs::places`). The first change writes the whole list.
//! - **Read** once at startup on the thread that reads the first folder (never the window's
//!   thread), with the settings. A file that can't be read or isn't valid gives the defaults
//!   and a note in the status bar; the next change replaces it.
//! - **Written** by one thread of its own ([`Writer`]), started by the first change and kept:
//!   the window sends it the new contents and goes on. It writes a temporary file and renames
//!   it over the old one, so a crash never leaves half a list. Several changes in a row are
//!   written once. A failed write is said once in the status bar. Quitting gives it up to
//!   100 ms to finish.
//! - **A favorite that's gone** shows dimmed, and a click says so instead of opening it. Each
//!   favorite is looked at once per launch, on a worker, after the window is up (not by the
//!   startup read): one `stat` per favorite, never repeated, never watched. One that's added is
//!   known to be there.
//! - **Editing:** "Add to Sidebar" (the selected folders, or the folder on screen), "Remove from
//!   Sidebar", "Move Up" and "Move Down" in an item's context menu, dragging an item up or down
//!   the list, and dropping folders on the sidebar (from another app, or rows from the list).
//!   ⌘1–9 follow the order.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender};

use rune::{Drag, Phase, Tx};

use crate::app::State;
use crate::fs;
use crate::persist;

/// The file's name, beside `config.toml`.
pub const FILE: &str = "sidebar.toml";

/// How far apart two sidebar rows are (26 pt rows, 1 pt gap): a drag that travels this far
/// moves the item one place.
pub const ROW_STEP: f32 = 27.0;

/// Where the favorites live.
pub fn file_path() -> Option<PathBuf> {
    Some(crate::config::path()?.with_file_name(FILE))
}

/// What the startup read found: the user's list, or nothing (no file: the defaults), and a
/// problem to show.
#[derive(Debug, Default, PartialEq)]
pub struct Loaded {
    pub list: Option<Vec<PathBuf>>,
    pub error: Option<String>,
}

/// Reads the favorites file at `path` (startup, off the window's thread).
pub fn load(path: Option<&Path>) -> Loaded {
    let Some(path) = path else { return Loaded::default() };
    match crate::config::read_text(path) {
        Ok(None) => Loaded::default(),
        Ok(Some(text)) => match parse(&text) {
            Ok(list) => Loaded { list: Some(list), error: None },
            Err(e) => Loaded { list: None, error: Some(corrupt(path, &e)) },
        },
        Err(e) => Loaded { list: None, error: Some(corrupt(path, &e)) },
    }
}

fn corrupt(path: &Path, why: &str) -> String {
    format!(
        "Sidebar: couldn't use {} ({why}); showing the default favorites. Your next change to the sidebar replaces it",
        fs::pretty(path)
    )
}

/// The list in `text`: `favorites = ["~/a", "/b"]`. A leading `~` is the home folder.
pub fn parse(text: &str) -> Result<Vec<PathBuf>, String> {
    let doc = toml_edit::Document::parse(text.to_string()).map_err(|e| {
        let at = e.span().map_or(0, |s| s.start).min(text.len());
        let line = text.as_bytes()[..at].iter().filter(|&&b| b == b'\n').count() + 1;
        format!("line {line}: {}", e.message().lines().next().unwrap_or("not valid TOML").trim().trim_end_matches('.'))
    })?;
    let Some(item) = doc.as_table().get("favorites") else { return Err("it has no favorites list".into()) };
    let Some(arr) = item.as_array() else { return Err("favorites isn't a list".into()) };
    let mut out: Vec<PathBuf> = Vec::new();
    for v in arr.iter() {
        let Some(s) = v.as_str() else { return Err("favorites has something that isn't a path".into()) };
        let p = fs::expand(s);
        if !p.is_absolute() {
            return Err(format!("“{s}” isn't a full path"));
        }
        if !out.contains(&p) {
            out.push(p);
        }
    }
    Ok(out)
}

/// The file's text for `paths`, or `None` when one of them isn't valid Unicode (TOML can't
/// hold it). Paths in the home folder are written with `~`.
pub fn serialize(paths: &[PathBuf]) -> Option<String> {
    let home = fs::home();
    let mut s = String::from(
        "# Arbor's sidebar favorites, in order. Arbor rewrites this file when you add, remove or move\n\
         # a favorite. Delete it to go back to the default list.\n\
         favorites = [\n",
    );
    for p in paths {
        let text = p.to_str()?;
        let shown = match p.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) if !home.as_os_str().is_empty() && home != Path::new("/") => format!("~/{}", rest.to_str()?.replace('\\', "/")),
            _ => text.to_string(),
        };
        s.push_str("    \"");
        for c in shown.chars() {
            match c {
                '"' => s.push_str("\\\""),
                '\\' => s.push_str("\\\\"),
                c if (c as u32) < 0x20 || c == '\u{7f}' => s.push_str(&format!("\\u{:04X}", c as u32)),
                c => s.push(c),
            }
        }
        s.push_str("\",\n");
    }
    s.push_str("]\n");
    Some(s)
}

/// A favorite's name in the sidebar: Home, Root, a drive (`C:`), or the folder's own name.
pub fn name_for(p: &Path) -> String {
    if p == fs::home() {
        return "Home".into();
    }
    match p.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        None if p == Path::new("/") => "Root".into(),
        None => p.display().to_string().trim_end_matches(['\\', '/']).to_string(),
    }
}

/// The favorites with their names.
pub fn named(paths: Vec<PathBuf>) -> Vec<(String, PathBuf)> {
    paths.into_iter().map(|p| (name_for(&p), p)).collect()
}

// ---- the writer ---------------------------------------------------------------------------

enum Msg {
    Save(String),
    /// Answered once everything sent before it is written.
    Flush(Sender<()>),
}

/// The one thread that writes `sidebar.toml`.
pub struct Writer {
    tx: Sender<Msg>,
    errors: Receiver<String>,
}

impl Writer {
    fn spawn(path: PathBuf, waker: Option<rune::Waker>) -> Writer {
        let (tx, rx) = std::sync::mpsc::channel::<Msg>();
        let (mut failures, errors) = persist::Failures::new(waker);
        persist::at_exit(&tx, Msg::Flush);
        std::thread::Builder::new()
            .name("arbor-sidebar".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    // Everything queued: only the newest list is written; flushes answer after.
                    let mut text = None;
                    let mut replies = Vec::new();
                    for m in std::iter::once(first).chain(std::iter::from_fn(|| rx.try_recv().ok())) {
                        match m {
                            Msg::Save(t) => text = Some(t),
                            Msg::Flush(r) => replies.push(r),
                        }
                    }
                    if let Some(t) = text {
                        let written = persist::replace(&path, t.as_bytes(), persist::Durability::Plain);
                        failures.note(written, |e| format!("Couldn't save the sidebar ({}): {e}", fs::pretty(&path)));
                    }
                    for r in replies {
                        let _ = r.send(());
                    }
                }
            })
            .expect("can't start the sidebar's writer");
        Writer { tx, errors }
    }

    fn save(&self, text: String) {
        let _ = self.tx.send(Msg::Save(text));
    }

    /// Waits up to `wait` for what's queued to be written. True when it was.
    #[cfg(test)]
    pub fn flush(&self, wait: std::time::Duration) -> bool {
        persist::flush(&self.tx, wait, Msg::Flush)
    }
}

/// A favorite's one look: its folder, and whether it's gone.
type Look = (PathBuf, bool);

// ---- the UI side --------------------------------------------------------------------------

/// The sidebar's state beyond the favorites themselves (`State::places`) and the volumes.
#[derive(Default)]
pub struct Side {
    /// The favorites come from `sidebar.toml` (not the defaults): a settings reload leaves
    /// them alone.
    pub custom: bool,
    /// Where the list is saved (`None`: nowhere, as in tests).
    pub file: Option<PathBuf>,
    writer: Option<Writer>,
    /// What each favorite's one look found (true: it's gone). Kept for the launch, so each is
    /// looked at once.
    pub missing: HashMap<PathBuf, bool>,
    /// Looks asked for (answered or not).
    looked: HashSet<PathBuf>,
    found: Option<(Sender<Look>, Receiver<Look>)>,
    /// A favorite being dragged: from where, to where.
    pub drag: Option<(usize, usize)>,
    /// The navigation a sidebar click asked for, so a folder that turns out gone is marked.
    pub(crate) going: Option<(u64, PathBuf)>,
    /// Folders dropped on the sidebar from another app, checked on a worker.
    pub(crate) dropped: rune::Latest<Vec<PathBuf>>,
    /// Looks started, for tests.
    pub looks: Arc<AtomicU64>,
    /// The favorites changed (or just landed): look at the new ones at the next `poll`.
    pub look_due: bool,
    /// Something is being dragged over the sidebar (folders to add).
    pub drop_hover: bool,
}

impl Side {
    pub fn new(file: Option<PathBuf>) -> Side {
        Side { file, ..Side::default() }
    }

    /// Whether the favorite at `p` is known to be gone.
    pub fn is_missing(&self, p: &Path) -> bool {
        self.missing.get(p).copied().unwrap_or(false)
    }

    pub(crate) fn writer(&mut self, waker: Option<rune::Waker>) -> Option<&Writer> {
        let path = self.file.clone()?;
        Some(self.writer.get_or_insert_with(|| Writer::spawn(path, waker)))
    }

    #[cfg(test)]
    pub fn writer_ref(&self) -> Option<&Writer> {
        self.writer.as_ref()
    }
}

impl State {
    /// The favorite or volume at sidebar index `i` (favorites first): its name and folder.
    pub fn place_at(&self, i: usize) -> Option<(String, PathBuf)> {
        match self.places.get(i) {
            Some(p) => Some(p.clone()),
            None => self.volumes.get(i - self.places.len()).map(|v| (v.name.clone(), v.path.clone())),
        }
    }

    /// How many sidebar items there are (favorites and volumes).
    pub fn place_count(&self) -> usize {
        self.places.len() + self.volumes.len()
    }

    /// A click on sidebar item `i` (or ⌘1–9): goes there, unless it's a favorite known to be
    /// gone, which says so.
    pub fn go_place(&mut self, i: usize) {
        let Some((name, path)) = self.place_at(i) else { return };
        if i < self.places.len() && self.side.is_missing(&path) {
            self.note = Some(gone(&name, &path));
            return;
        }
        if path == self.cur().cwd {
            return;
        }
        self.go(path.clone(), crate::folder::Nav::New, None);
        if i < self.places.len()
            && let Some(req) = self.cur().pending.as_ref().map(|p| p.req)
        {
            self.side.going = Some((req, path));
        }
    }

    /// A sidebar click's folder turned out not to be there: dimmed from now on, and said.
    pub(crate) fn place_missed(&mut self, path: &Path) {
        self.side.missing.insert(path.to_path_buf(), true);
        self.side.looked.insert(path.to_path_buf());
        let name = self.places.iter().find(|(_, p)| p == path).map_or_else(|| name_for(path), |(n, _)| n.clone());
        self.note = Some(gone(&name, path));
    }

    /// Looks once at each favorite not looked at yet this launch, each on a worker of its own
    /// (a dead network folder holds up only its own look), from `poll` once the window is up.
    pub(crate) fn look_at_favorites(&mut self) {
        let todo: Vec<PathBuf> = self.places.iter().map(|(_, p)| p.clone()).filter(|p| !self.side.looked.contains(p)).collect();
        if todo.is_empty() {
            return;
        }
        let (tx, _) = self.side.found.get_or_insert_with(std::sync::mpsc::channel);
        let tx = tx.clone();
        for p in todo {
            self.side.looked.insert(p.clone());
            self.side.looks.fetch_add(1, Relaxed);
            let (tx, waker) = (tx.clone(), self.waker.clone());
            std::thread::spawn(move || {
                let gone = !p.is_dir();
                if tx.send((p, gone)).is_ok()
                    && let Some(w) = waker
                {
                    w();
                }
            });
        }
    }

    /// The favorites' looks that answered, the writer's failures and dropped folders, from
    /// `poll`. True when anything changed.
    pub(crate) fn poll_sidebar(&mut self) -> bool {
        let mut changed = false;
        if let Some((_, rx)) = &self.side.found {
            while let Ok((p, gone)) = rx.try_recv() {
                changed |= self.side.missing.insert(p, gone) != Some(gone);
            }
        }
        if let Some(e) = self.side.writer.as_ref().and_then(|w| w.errors.try_recv().ok()) {
            self.note = Some(e);
            self.alert = self.note.clone();
            changed = true;
        }
        if let Some(dirs) = self.side.dropped.take() {
            if dirs.is_empty() {
                self.note = Some("Only folders can go in the sidebar".into());
            } else {
                self.add_favorites(dirs, None);
            }
            changed = true;
        }
        changed
    }

    /// Saves the favorites, on the writer's thread.
    fn save_favorites(&mut self) {
        let paths: Vec<PathBuf> = self.places.iter().map(|(_, p)| p.clone()).collect();
        let Some(text) = serialize(&paths) else {
            self.note = Some("Couldn't save the sidebar: a folder's name isn't valid Unicode".into());
            return;
        };
        self.side.custom = true;
        let waker = self.waker.clone();
        if let Some(w) = self.side.writer(waker) {
            w.save(text);
        }
    }

    /// Adds `dirs` (folders, known to be there) to the favorites, at `at` or the end; those
    /// already there stay where they are.
    pub(crate) fn add_favorites(&mut self, dirs: Vec<PathBuf>, at: Option<usize>) {
        let new: Vec<PathBuf> = dirs.into_iter().filter(|d| !self.places.iter().any(|(_, p)| p == d)).fold(Vec::new(), |mut v, d| {
            if !v.contains(&d) {
                v.push(d);
            }
            v
        });
        if new.is_empty() {
            self.note = Some("Already in the sidebar".into());
            return;
        }
        let at = at.unwrap_or(self.places.len()).min(self.places.len());
        for p in &new {
            self.side.missing.insert(p.clone(), false);
            self.side.looked.insert(p.clone());
        }
        let n = new.len();
        let first = name_for(&new[0]);
        self.places.splice(at..at, named(new));
        self.note = Some(if n == 1 { format!("Added “{first}” to the sidebar") } else { format!("Added {n} folders to the sidebar") });
        self.save_favorites();
    }

    /// "Add to Sidebar": the selected folders, or the folder on screen when no folder is
    /// selected (from the listing: no disk I/O).
    pub fn add_to_sidebar(&mut self) {
        let f = self.cur();
        let picked: Vec<PathBuf> = f.targets().iter().filter(|e| e.is_dir).map(|e| f.cwd.join(&e.os)).collect();
        let dirs = if picked.is_empty() { vec![f.cwd.clone()] } else { picked };
        self.add_favorites(dirs, None);
    }

    /// "Remove from Sidebar" for favorite `i`. Only the sidebar changes; the folder stays.
    pub fn remove_favorite(&mut self, i: usize) {
        if i >= self.places.len() {
            return;
        }
        let (name, _) = self.places.remove(i);
        self.note = Some(format!("Removed “{name}” from the sidebar (the folder itself is untouched)"));
        self.save_favorites();
    }

    /// The favorite showing the folder on screen, for "Remove from Sidebar" from the palette.
    pub fn favorite_here(&self) -> Option<usize> {
        self.places.iter().position(|(_, p)| *p == self.cur().cwd)
    }

    /// Moves favorite `from` to `to`.
    pub fn move_favorite(&mut self, from: usize, to: usize) {
        let n = self.places.len();
        if from >= n || to >= n || from == to {
            return;
        }
        let item = self.places.remove(from);
        self.places.insert(to, item);
        self.save_favorites();
    }

    /// A drag of favorite `i`: the place it would land shows while it moves (one place per
    /// row's height travelled), and letting go there moves it.
    pub fn drag_favorite(&mut self, i: usize, d: &Drag, tx: &mut Tx) {
        self.drag_favorite_by(i, d.phase, d.dy, d.cancelled(), tx);
    }

    /// [`State::drag_favorite`], from the drag's phase, its vertical travel and whether it was
    /// cancelled.
    pub(crate) fn drag_favorite_by(&mut self, i: usize, phase: Phase, dy: f32, cancelled: bool, tx: &mut Tx) {
        tx.instant();
        let n = self.places.len();
        if i >= n {
            return;
        }
        let to = (i as f32 + (dy / ROW_STEP).round()).clamp(0.0, (n - 1) as f32) as usize;
        if phase == Phase::Up {
            self.side.drag = None;
            if !cancelled && to != i {
                self.move_favorite(i, to);
            }
        } else if self.side.drag != Some((i, to)) {
            self.side.drag = Some((i, to));
        } else {
            tx.live_only();
        }
    }

    /// Files dropped on the sidebar from another app: the folders among them are added, once a
    /// worker has looked at which are folders.
    pub fn drop_on_sidebar(&mut self, files: &[PathBuf]) {
        let files = files.to_vec();
        self.side.dropped.request(files, |files| files.into_iter().filter(|p| p.is_dir()).collect());
    }

    /// Rows dragged from pane `id`'s list onto the sidebar: their folders are added (what the
    /// listing says, no disk I/O).
    pub fn drop_rows_on_sidebar(&mut self, id: u64, i: usize, version: u64) {
        let Some(p) = self.pane(id).filter(|p| p.f.version == version) else { return };
        let f = &p.f;
        let rows = if f.is_selected(i) { f.targets() } else { f.visible.get(i).map(|&e| &f.entries[e as usize]).into_iter().collect() };
        let dirs: Vec<PathBuf> = rows.iter().filter(|e| e.is_dir).map(|e| f.cwd.join(&e.os)).collect();
        if dirs.is_empty() {
            self.note = Some("Only folders can go in the sidebar".into());
            return;
        }
        self.add_favorites(dirs, None);
    }
}

/// What a click on a favorite that's gone says.
fn gone(name: &str, path: &Path) -> String {
    format!("“{name}” isn't there any more ({}). Right-click it to remove it from the sidebar", fs::pretty(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{settle_all, state_in};
    use std::time::Duration;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-sidebar-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_file_round_trips_with_home_as_a_tilde() {
        let home = fs::home();
        let list = vec![home.clone(), home.join("Projects"), PathBuf::from(if cfg!(windows) { r"D:\Data" } else { "/srv/a \"b\"" })];
        let text = serialize(&list).unwrap();
        assert!(text.contains("\"~\",") && text.contains("\"~/Projects\","), "{text}");
        assert_eq!(parse(&text).unwrap(), list);
    }

    #[test]
    fn a_bad_file_is_an_error_not_a_guess() {
        assert!(parse("favorites = [\"/a\", 3]").is_err());
        assert!(parse("favorites = \"/a\"").is_err());
        assert!(parse("nothing = 1").is_err());
        assert!(parse("favorites = [\"relative/path\"]").is_err());
        assert!(parse("favorites = [\n\"/a\",\n").unwrap_err().starts_with("line "));
        assert_eq!(parse("favorites = []").unwrap(), Vec::<PathBuf>::new());
        let a = if cfg!(windows) { "C:/a" } else { "/a" };
        assert_eq!(parse(&format!("favorites = [\"{a}\", \"{a}\"]")).unwrap(), vec![PathBuf::from(a)], "duplicates once");
    }

    /// A corrupt file gives the defaults (no list) and a message; a missing one gives the
    /// defaults quietly; a good one gives its list.
    #[test]
    fn a_corrupt_file_means_the_defaults_and_a_message() {
        let d = temp("load");
        let p = d.join(FILE);
        assert_eq!(load(Some(&p)), Loaded::default());
        std::fs::write(&p, "favorites = [\"/a\",").unwrap();
        let l = load(Some(&p));
        assert!(l.list.is_none());
        assert!(l.error.as_deref().is_some_and(|e| e.contains("default favorites") && e.contains("line")), "{l:?}");
        std::fs::write(&p, serialize(std::slice::from_ref(&d)).unwrap()).unwrap();
        assert_eq!(load(Some(&p)), Loaded { list: Some(vec![d.clone()]), error: None });
        let _ = std::fs::remove_dir_all(&d);
    }

    fn state_with(d: &Path, file: &Path, favs: &[PathBuf]) -> State {
        let mut s = state_in(d);
        settle_all(&mut s);
        s.places = named(favs.to_vec());
        s.side.file = Some(file.to_path_buf());
        s
    }

    fn saved(s: &State, file: &Path) -> Vec<PathBuf> {
        assert!(s.side.writer_ref().unwrap().flush(Duration::from_secs(5)));
        parse(&std::fs::read_to_string(file).unwrap()).unwrap()
    }

    /// Add, remove and reorder each save the list on the writer's thread, in order.
    #[test]
    fn add_remove_and_reorder_are_saved() {
        let root = temp("edit");
        let (d, favs) = (root.join("here"), root.join("favs"));
        let (a, b, c) = (favs.join("a"), favs.join("b"), favs.join("c"));
        for x in [&a, &b, &c, &d] {
            std::fs::create_dir_all(x).unwrap();
        }
        std::fs::write(d.join("f.txt"), "x").unwrap();
        let file = root.join("cfg").join(FILE);
        let mut s = state_with(&d, &file, std::slice::from_ref(&a));
        assert!(s.side.writer_ref().is_none(), "nothing written until a change");
        // The folder on screen (a file is selected).
        s.add_to_sidebar();
        assert_eq!(saved(&s, &file), vec![a.clone(), d.clone()]);
        // Selected folders.
        s.go(favs.clone(), crate::folder::Nav::New, None);
        settle_all(&mut s);
        let i = s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].os == "b").unwrap();
        s.cur_mut().select(i);
        s.add_to_sidebar();
        assert_eq!(s.places.last().map(|p| p.0.as_str()), Some("b"));
        s.add_to_sidebar();
        assert_eq!(s.note.as_deref(), Some("Already in the sidebar"));
        s.add_favorites(vec![c.clone()], Some(0));
        assert_eq!(saved(&s, &file), vec![c.clone(), a.clone(), d.clone(), b.clone()]);
        s.move_favorite(0, 3);
        assert_eq!(saved(&s, &file), vec![a.clone(), d.clone(), b.clone(), c.clone()]);
        s.remove_favorite(1);
        assert_eq!(saved(&s, &file), vec![a.clone(), b.clone(), c.clone()]);
        assert!(d.exists(), "removing a favorite leaves the folder");
        assert!(s.side.custom, "the list is the user's now");
        // ⌘1–9 follow the order.
        assert_eq!(s.title(crate::commands::Cmd::Place2), "Go to b");
        let mut tx = Tx::default();
        s.run(crate::commands::Cmd::Place3, &mut tx);
        settle_all(&mut s);
        assert_eq!(s.cur().cwd, c);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Dragging an item down two rows moves it two places; a cancelled drag moves nothing.
    #[test]
    fn dragging_a_favorite_reorders() {
        let d = temp("drag");
        let file = d.join(FILE);
        let favs: Vec<PathBuf> = ["a", "b", "c", "d"].iter().map(|n| d.join(n)).collect();
        let mut s = state_with(&d, &file, &favs);
        let mut tx = Tx::default();
        s.drag_favorite_by(0, Phase::Down, 0.0, false, &mut tx);
        s.drag_favorite_by(0, Phase::Move, ROW_STEP * 2.2, false, &mut tx);
        assert_eq!(s.side.drag, Some((0, 2)));
        s.drag_favorite_by(0, Phase::Up, ROW_STEP * 2.2, true, &mut tx);
        assert_eq!((s.side.drag, s.places[0].1.clone()), (None, favs[0].clone()), "cancelled: nothing moves");
        s.drag_favorite_by(0, Phase::Up, ROW_STEP * 2.2, false, &mut tx);
        assert_eq!(saved(&s, &file), vec![favs[1].clone(), favs[2].clone(), favs[0].clone(), favs[3].clone()]);
        // Past the end clamps to the last place.
        s.drag_favorite_by(1, Phase::Up, ROW_STEP * 40.0, false, &mut tx);
        assert_eq!(s.places.last().unwrap().1, favs[2]);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Each favorite is looked at once, on a worker; one that's gone is dimmed, and a click
    /// says so rather than going there. A second look never happens in the same launch.
    #[test]
    fn a_missing_favorite_is_found_lazily_once() {
        let d = temp("missing");
        let here = d.join("here");
        std::fs::create_dir_all(&here).unwrap();
        let gone = d.join("gone");
        let mut s = state_with(&d, &d.join(FILE), &[here.clone(), gone.clone()]);
        assert!(s.side.missing.is_empty(), "nothing looked at until asked");
        s.look_at_favorites();
        s.look_at_favorites();
        assert_eq!(s.side.looks.load(Relaxed), 2, "one look each");
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while s.side.missing.len() < 2 && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
            s.poll_sidebar();
        }
        assert!(!s.side.is_missing(&here) && s.side.is_missing(&gone));
        let at = s.cur().cwd.clone();
        s.go_place(1);
        assert_eq!(s.cur().cwd, at, "didn't go");
        assert!(!s.is_loading());
        assert!(s.note.as_deref().is_some_and(|n| n.contains("isn't there any more")), "{:?}", s.note);
        // Made again: a later look would find it, but there's none this launch.
        std::fs::create_dir_all(&gone).unwrap();
        s.look_at_favorites();
        assert_eq!(s.side.looks.load(Relaxed), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Building the window looks at no favorite (the startup read only reads the file, off this
    /// thread); the first `poll` after the first listing starts one look per favorite.
    #[test]
    fn startup_looks_at_nothing_until_the_window_is_up() {
        let d = temp("startup");
        let mut s = state_in(&d);
        s.places = named(vec![d.clone(), d.join("x")]);
        s.side.look_due = true;
        assert_eq!(s.side.looks.load(Relaxed), 0);
        assert!(s.side.writer_ref().is_none());
        s.poll(&mut Tx::default());
        assert_eq!(s.side.looks.load(Relaxed), 2);
        s.poll(&mut Tx::default());
        assert_eq!(s.side.looks.load(Relaxed), 2, "once per launch");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A favorite that vanished before its look: the click's failed read marks it.
    #[test]
    fn a_click_on_a_vanished_favorite_marks_it() {
        let d = temp("vanished");
        let gone = d.join("gone");
        let mut s = state_with(&d, &d.join(FILE), std::slice::from_ref(&gone));
        s.go_place(0);
        settle_all(&mut s);
        assert!(s.side.is_missing(&gone));
        assert!(s.note.as_deref().is_some_and(|n| n.contains("isn't there any more")), "{:?}", s.note);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Folders dropped from another app are added once a worker has looked; files aren't.
    #[test]
    fn dropped_folders_are_added() {
        let d = temp("drop");
        let sub = d.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(d.join("f.txt"), "x").unwrap();
        let file = d.join(FILE);
        let mut s = state_with(&d, &file, &[]);
        s.drop_on_sidebar(&[sub.clone(), d.join("f.txt")]);
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while s.places.is_empty() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
            s.poll_sidebar();
        }
        assert_eq!(saved(&s, &file), vec![sub]);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A write that fails (the folder is a file) is said once in the status bar.
    #[test]
    fn a_failed_save_is_reported() {
        let d = temp("fail");
        std::fs::write(d.join("blocker"), "x").unwrap();
        let file = d.join("blocker").join(FILE);
        let mut s = state_with(&d, &file, &[]);
        s.add_to_sidebar();
        assert!(s.side.writer_ref().unwrap().flush(Duration::from_secs(5)));
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while !s.note.as_deref().is_some_and(|n| n.starts_with("Couldn't save the sidebar")) && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
            s.poll_sidebar();
        }
        assert!(s.note.as_deref().is_some_and(|n| n.starts_with("Couldn't save the sidebar")), "{:?}", s.note);
        let _ = std::fs::remove_dir_all(&d);
    }
}
