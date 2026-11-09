//! Tab completion in go-to mode (`/…`, `~/…`, `C:\…` typed in the status bar): the typed path's
//! last segment against the entries of the folder before it.
//!
//! The first Tab completes as far as every candidate agrees (all of a single one, with a
//! separator after a folder) and, with several, lists them in a small card over the status
//! bar; Tab again cycles through them (⇧Tab backwards). Typing anything else ends it.
//!
//! The folder is read on a worker for each completion, as showing a tab is: the key waits at
//! most a frame for it (`State::show_wait`), and a slower answer applies when it lands if the
//! field hasn't changed since. Nothing is kept: each Tab, cycling too, reads it again. Capitals count
//! on Linux and don't on macOS and Windows, as their usual file systems do. Hidden items are
//! candidates while hidden files are shown, or once what's typed starts with a dot (as shells
//! do).

use rune::{Latest, Tx};

use crate::app::State;

/// A completion in progress, while its card shows.
pub struct Completion {
    /// The pane whose field it completes.
    pub pane: u64,
    /// What's typed up to and including the last separator: the folder, as typed.
    pub dir: String,
    /// The start of the name that was completed (each Tab reads the folder for it again).
    pub prefix: String,
    /// The matching names, sorted, a folder's with a separator after it.
    pub cands: Vec<String>,
    /// The candidate shown in the field while cycling (`None` right after the first Tab).
    pub at: Option<usize>,
    /// The field's text as this completion left it: any other text means the user typed since,
    /// and it's over.
    pub shown: String,
}

/// A completion the worker answered: the field's text it was asked for, and its candidates.
pub type Answer = (String, Vec<String>);

/// The worker that reads folders for completion.
pub type Loader = Latest<Answer>;

/// Whether capitals count when matching names: as each platform's usual file system does.
pub const CASE_SENSITIVE: bool = cfg!(not(any(target_os = "macos", windows)));

/// Whether `c` separates folders in a typed path on Windows (`windows`) or elsewhere.
fn is_sep_on(c: char, windows: bool) -> bool {
    c == '/' || (windows && c == '\\')
}

fn is_sep(c: char) -> bool {
    is_sep_on(c, cfg!(windows))
}

/// The typed path split after its last separator: the folder as typed, and the start of the
/// name to complete.
pub fn split(typed: &str) -> (&str, &str) {
    split_on(typed, cfg!(windows))
}

/// [`split`] as on Windows (`windows`: `\` separates too, so `C:\Users\me` and
/// `\\server\share\x` split) or elsewhere (a backslash is part of a name): pure, so both are
/// tested everywhere.
pub fn split_on(typed: &str, windows: bool) -> (&str, &str) {
    match typed.char_indices().rev().find(|&(_, c)| is_sep_on(c, windows)) {
        Some((i, c)) => typed.split_at(i + c.len_utf8()),
        None => ("", typed),
    }
}

/// Whether `name` starts with `prefix` under the case rule.
pub fn starts_with(name: &str, prefix: &str, sensitive: bool) -> bool {
    if sensitive {
        return name.starts_with(prefix);
    }
    let mut n = name.chars().flat_map(char::to_lowercase);
    prefix.chars().flat_map(char::to_lowercase).all(|p| n.next() == Some(p))
}

/// What every candidate starts with, in the first one's spelling (capitals aside when they
/// don't count).
pub fn common_prefix(cands: &[String], sensitive: bool) -> String {
    let Some(first) = cands.first() else { return String::new() };
    let same = |a: char, b: char| if sensitive { a == b } else { a.to_lowercase().eq(b.to_lowercase()) };
    let mut end = first.len();
    for c in &cands[1..] {
        let common = first.char_indices().zip(c.chars()).take_while(|&((_, a), b)| same(a, b)).last();
        end = end.min(common.map_or(0, |((i, a), _)| i + a.len_utf8()));
    }
    first[..end].to_string()
}

/// The names in `dir` that start with `prefix`, sorted (capitals aside), each folder's
/// (including a link to one) with `sep` after it. Hidden items only with `hidden`, or when
/// `prefix` starts with a dot. Runs on a
/// worker. An unreadable folder has none.
pub fn candidates(dir: &std::path::Path, prefix: &str, hidden: bool, sensitive: bool, sep: char) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    // A dot typed asks for hidden items, as in a shell.
    let hidden = hidden || prefix.starts_with('.');
    let mut out: Vec<(String, bool)> = Vec::new();
    for de in rd.flatten() {
        let name = de.file_name().to_string_lossy().into_owned();
        if !starts_with(&name, prefix, sensitive) {
            continue;
        }
        // Only a match is looked at further: its hidden flag (a dot, or the platform's flag)
        // and whether it's a folder.
        if !hidden && crate::fs::is_hidden(&name, de.metadata().ok().as_ref()) {
            continue;
        }
        let ft = de.file_type().ok();
        let is_dir = ft.is_some_and(|t| t.is_dir()) || (ft.is_some_and(|t| t.is_symlink()) && de.path().is_dir());
        out.push((name, is_dir));
    }
    out.sort_by_cached_key(|(n, _)| (n.to_lowercase(), n.clone()));
    out.into_iter().map(|(n, dir)| if dir { format!("{n}{sep}") } else { n }).collect()
}

/// Where Tab (or ⇧Tab, `back`) goes from `current` in `cands` (sorted as `candidates` sorts):
/// the next one, wrapping; the first (or last) when nothing is shown yet; and when `current`
/// is gone, the one that sorts after (or before) where it was.
fn next(cands: &[String], current: Option<&str>, back: bool) -> usize {
    let n = cands.len();
    let key = |s: &str| (s.to_lowercase(), s.to_string());
    match (current, back) {
        (None, false) => 0,
        (None, true) => n - 1,
        (Some(c), _) => match cands.iter().position(|x| x == c) {
            Some(i) if back => (i + n - 1) % n,
            Some(i) => (i + 1) % n,
            None if back => cands.iter().rposition(|x| key(x) < key(c)).unwrap_or(n - 1),
            None => cands.iter().position(|x| key(x) > key(c)).unwrap_or(0),
        },
    }
}

/// The separator to put after a completed folder: the one the typed path uses last, else the
/// platform's.
fn sep_for(dir: &str) -> char {
    dir.chars().rev().find(|&c| is_sep(c)).unwrap_or(std::path::MAIN_SEPARATOR)
}

impl State {
    /// Tab in go-to mode (`back`: ⇧Tab).
    pub(crate) fn complete_tab(&mut self, back: bool, tx: &mut Tx) {
        tx.instant();
        let (pane, typed) = (self.cur().id, self.cur().filter.clone());
        // Tab again: the next candidate, from the folder read again (a name removed since is
        // skipped, one added joins), waiting at most a frame as the first Tab does; a slower
        // folder cycles through what the last read found, and its answer applies when it lands
        // (`cycled_late`).
        if let Some(c) = self.complete.as_ref().filter(|c| c.pane == pane && c.shown == typed && c.cands.len() > 1) {
            let (dir, prefix, at, old) = (c.dir.clone(), c.prefix.clone(), c.at, c.cands.clone());
            let current = at.and_then(|i| old.get(i).cloned());
            let (folder, sep, hidden) = (crate::fs::expand(&dir), sep_for(&dir), self.show_hidden);
            let fresh = crate::bounded::read_waiting(&self.completer, self.show_wait, move || {
                Some((String::new(), candidates(&folder, &prefix, hidden, CASE_SENSITIVE, sep)))
            });
            let cands = fresh.map_or(old, |(_, c)| c);
            match cands.as_slice() {
                [] => {
                    self.complete = None;
                    self.note = Some(format!("Nothing in {dir} starts with “{}” any more", split(&typed).1));
                }
                [one] => {
                    self.complete = None;
                    self.cur_mut().filter = format!("{dir}{one}");
                }
                _ => {
                    let at = next(&cands, current.as_deref(), back);
                    let shown = format!("{dir}{}", cands[at]);
                    if let Some(c) = self.complete.as_mut() {
                        c.cands = cands;
                        c.at = Some(at);
                        c.shown = shown.clone();
                    }
                    self.cur_mut().filter = shown;
                }
            }
            return;
        }
        self.complete = None;
        if typed == "~" {
            self.cur_mut().filter = format!("~{}", std::path::MAIN_SEPARATOR);
            return;
        }
        let (dir, prefix) = split(&typed);
        let (folder, prefix, sep) = (crate::fs::expand(dir), prefix.to_string(), sep_for(dir));
        let hidden = self.show_hidden;
        let asked = typed.clone();
        let answer = crate::bounded::read_waiting(&self.completer, self.show_wait, move || {
            Some((asked, candidates(&folder, &prefix, hidden, CASE_SENSITIVE, sep)))
        });
        match answer {
            Some(a) => self.completed(a),
            None => self.completing = Some(typed),
        }
    }

    /// A completion's candidates: applied if the field still says what it was asked for.
    pub(crate) fn completed(&mut self, (typed, cands): Answer) {
        self.completing = None;
        let f = self.cur();
        if f.filter != typed || !f.path_mode() {
            return;
        }
        let pane = f.id;
        let (dir, prefix) = split(&typed);
        let (dir, prefix) = (dir.to_string(), prefix.to_string());
        let prefix_typed = prefix.clone();
        match cands.as_slice() {
            [] => {
                let folder = if dir.is_empty() { typed.clone() } else { dir.clone() };
                self.note = Some(format!("Nothing in {folder} starts with “{prefix}”"));
            }
            [one] => self.cur_mut().filter = format!("{dir}{one}"),
            _ => {
                let common = common_prefix(&cands, CASE_SENSITIVE);
                // As far as they all agree, in their spelling; never shorter than what's typed.
                let name = if common.chars().count() > prefix.chars().count() { common } else { prefix };
                let shown = format!("{dir}{name}");
                self.cur_mut().filter = shown.clone();
                self.complete = Some(Completion { pane, dir, prefix: prefix_typed, cands, at: None, shown });
            }
        }
    }

    /// A late completion answer, from `poll`. True when it changed something.
    pub(crate) fn poll_completion(&mut self) -> bool {
        let Some(a) = self.completer.take() else { return false };
        // A cycling Tab's read (asked with no field text): the candidates it found.
        if a.0.is_empty() {
            return self.cycled_late(a.1);
        }
        if self.completing.as_ref() != Some(&a.0) {
            return false;
        }
        self.completed(a);
        true
    }

    /// A cycling Tab's fresh read that came after the key, while its card still shows: the
    /// candidates become what the folder holds now, and a name shown that's gone since gives
    /// way to the one after it. True when it changed something.
    fn cycled_late(&mut self, cands: Vec<String>) -> bool {
        let Some(c) = self.completion() else { return false };
        let (dir, current) = (c.dir.clone(), c.at.and_then(|i| c.cands.get(i).cloned()));
        if c.cands == cands {
            return false;
        }
        match cands.as_slice() {
            [] => {
                self.complete = None;
                self.note = Some(format!("Nothing in {dir} starts with “{}” any more", current.as_deref().map_or("", |s| s)));
            }
            [one] => {
                self.complete = None;
                self.cur_mut().filter = format!("{dir}{one}");
            }
            _ => {
                let at = match current.as_deref() {
                    Some(cur) => cands.iter().position(|x| x == cur).unwrap_or_else(|| next(&cands, Some(cur), false)),
                    None => 0,
                };
                let shown = if current.is_some() { format!("{dir}{}", cands[at]) } else { self.cur().filter.clone() };
                if let Some(c) = self.complete.as_mut() {
                    c.cands = cands;
                    c.at = current.is_some().then_some(at);
                    c.shown = shown.clone();
                }
                self.cur_mut().filter = shown;
            }
        }
        true
    }

    /// The completion whose card shows now: the active pane's, while its field still says what
    /// the completion left there.
    pub fn completion(&self) -> Option<&Completion> {
        let f = self.cur();
        self.complete.as_ref().filter(|c| c.pane == f.id && c.shown == f.filter && f.path_mode())
    }
}

/// The folder a completion reads, for tests.
#[cfg(test)]
pub(crate) fn folder_of(typed: &str) -> std::path::PathBuf {
    crate::fs::expand(split(typed).0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{settle_all, state_in};
    use rune::{Key, KeyEvent, Mods};

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-complete-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_typed_path_splits_after_its_last_separator() {
        assert_eq!(split("/usr/lo"), ("/usr/", "lo"));
        assert_eq!(split("/usr/"), ("/usr/", ""));
        assert_eq!(split("~/Doc"), ("~/", "Doc"));
        assert_eq!(split("/"), ("/", ""));
        if cfg!(windows) {
            assert_eq!(split(r"C:\Users\me\Doc"), (r"C:\Users\me\", "Doc"));
            assert_eq!(split(r"C:\Users/x"), (r"C:\Users/", "x"));
        } else {
            assert_eq!(split(r"/a\b"), ("/", r"a\b"), "a backslash is part of a name on Unix");
        }
        assert_eq!(folder_of("~/Doc"), crate::fs::home().join(""));
    }

    const TYPED: &[&str] = &["/", "\\", "~", "~/", "~\\", "C:", "C:\\", "\\\\server\\share\\", "..", ".", "a", "é", " ", "Users", "me"];

    /// Any typed path, as on Windows and elsewhere: the folder and the name put back together
    /// are what was typed; the name holds no separator; the folder is nothing or ends in one.
    #[test]
    fn any_typed_path_splits_into_its_folder_and_a_name() {
        crate::prop::check("goto_split", 5000, |r| {
            let typed = if r.one_in(4) { r.string(20) } else { r.from(TYPED, 8) };
            for windows in [false, true] {
                let (dir, name) = split_on(&typed, windows);
                assert_eq!(format!("{dir}{name}"), typed);
                assert!(!name.chars().any(|c| is_sep_on(c, windows)), "{typed:?} {windows}");
                assert!(dir.is_empty() || dir.chars().last().is_some_and(|c| is_sep_on(c, windows)), "{typed:?} {windows}");
            }
        });
    }

    /// Windows' drive and UNC syntax, on every OS: they split at backslashes as Windows does,
    /// and only there; a drive starts a path only there.
    #[test]
    fn windows_drives_and_shares_split_on_every_os() {
        assert_eq!(split_on(r"C:\Users\me\Doc", true), (r"C:\Users\me\", "Doc"));
        assert_eq!(split_on(r"\\server\share\dir\fi", true), (r"\\server\share\dir\", "fi"));
        assert_eq!(split_on(r"C:/Users\x", true), (r"C:/Users\", "x"));
        assert_eq!(split_on("C:", true), ("", "C:"));
        assert_eq!(split_on(r"\\server\share\dir\fi", false), ("", r"\\server\share\dir\fi"), "a backslash is a name's elsewhere");
        assert!(crate::app::is_path_like_on("C:\\x", true) && !crate::app::is_path_like_on("C:\\x", false));
        assert!(crate::app::is_path_like_on(r"\\server\share", true) && crate::app::is_path_like_on("~", false));
        assert!(!crate::app::is_path_like_on("report.txt", true) && !crate::app::is_path_like_on("1:", false));
        assert_eq!(sep_for(r"C:\Users\"), if cfg!(windows) { '\\' } else { std::path::MAIN_SEPARATOR });
    }

    /// Found reading the go-to code: the typed path was trimmed, so a folder whose name ends in
    /// a space (Tab completion writes it whole) went to the item without the space, and a file
    /// there was opened. Now the exact path comes first; a stray space still finds its folder.
    #[cfg(unix)]
    #[test]
    fn go_to_a_name_ending_in_a_space_goes_there() {
        let d = tmp("trailing-space");
        std::fs::write(d.join("report"), "another item").unwrap();
        std::fs::create_dir(d.join("report ")).unwrap();
        let mut s = state_in(&d);
        settle_all(&mut s);
        s.cur_mut().filter = format!("{}/report ", d.display());
        s.on_key(&KeyEvent { key: Key::Enter, mods: Mods::default() }, &mut rune::Tx::default());
        settle_all(&mut s);
        assert_eq!(s.cur().cwd, d.join("report "), "the folder with the space");
        // A stray space after a name that's there.
        std::fs::create_dir(d.join("sub")).unwrap();
        s.cur_mut().filter = format!("{}/sub  ", d.display());
        s.on_key(&KeyEvent { key: Key::Enter, mods: Mods::default() }, &mut rune::Tx::default());
        settle_all(&mut s);
        assert_eq!(s.cur().cwd, d.join("sub"));
        assert_eq!(crate::app::typed_targets(" /a b "), [std::path::PathBuf::from("/a b "), std::path::PathBuf::from("/a b")]);
    }

    #[test]
    fn matching_and_the_common_prefix_follow_the_case_rule() {
        assert!(starts_with("Documents", "Doc", true) && !starts_with("Documents", "doc", true));
        assert!(starts_with("Documents", "doc", false) && starts_with("ÉTÉ", "été", false));
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(common_prefix(&v(&["Documents/", "Downloads/"]), true), "Do");
        assert_eq!(common_prefix(&v(&["report-2024.pdf", "report-2025.pdf"]), true), "report-202");
        assert_eq!(common_prefix(&v(&["Photos/", "photos-old/"]), true), "");
        assert_eq!(common_prefix(&v(&["Photos/", "photos-old/"]), false), "Photos", "the first one's spelling");
        assert_eq!(common_prefix(&v(&["naïve", "naïf"]), true), "naï", "whole characters");
        assert_eq!(common_prefix(&v(&["only"]), true), "only");
    }

    #[test]
    fn candidates_are_sorted_with_folders_marked_and_hidden_ones_on_request() {
        let d = tmp("cands");
        for f in ["notes.txt", "Notebook.md", ".notes-secret", "other"] {
            std::fs::write(d.join(f), "x").unwrap();
        }
        std::fs::create_dir(d.join("notes")).unwrap();
        let got = candidates(&d, "no", false, true, '/');
        assert_eq!(got, ["notes/", "notes.txt"], "Linux rule: capitals count");
        let got = candidates(&d, "no", false, false, '/');
        assert_eq!(got, ["Notebook.md", "notes/", "notes.txt"], "macOS and Windows rule");
        assert_eq!(candidates(&d, ".no", false, true, '/'), [".notes-secret"], "a dot asks for hidden ones");
        assert_eq!(candidates(&d, "", true, true, '/').len(), 5);
        assert_eq!(candidates(&d, "", false, true, '/').len(), 4);
        assert!(candidates(&d.join("missing"), "", true, true, '/').is_empty());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(d.join("notes"), d.join("link-to-notes")).unwrap();
            assert_eq!(candidates(&d, "link", false, true, '/'), ["link-to-notes/"], "a link to a folder is a folder");
        }
    }

    fn tab(shift: bool) -> KeyEvent {
        KeyEvent { key: Key::Tab, mods: Mods { shift, ..Mods::default() } }
    }

    /// Types `path` into the go-to field, as the keys would.
    fn type_path(s: &mut State, path: &str) {
        let f = s.cur_mut();
        f.filter.clear();
        f.type_filter(path);
    }

    /// The review's case: a candidate removed between Tabs used to be cycled to anyway (one
    /// read for the whole completion). Each Tab reads the folder again, so a removed name is
    /// skipped, one that appears joins, and the last one left completes whole.
    #[test]
    fn cycling_reads_the_folder_again_each_tab() {
        let d = tmp("cycle-fresh");
        for n in ["Photographs", "Photos", "Pictures old"] {
            std::fs::create_dir(d.join(n)).unwrap();
        }
        let mut s = state_in(&d);
        settle_all(&mut s);
        let mut tx = Tx::default();
        let base = format!("{}/", d.display());
        type_path(&mut s, &format!("{base}P"));
        s.on_key(&tab(false), &mut tx);
        s.on_key(&tab(false), &mut tx);
        assert_eq!(s.cur().filter, format!("{base}Photographs/"));
        std::fs::remove_dir(d.join("Photos")).unwrap();
        s.on_key(&tab(false), &mut tx);
        assert_eq!(s.cur().filter, format!("{base}Pictures old/"), "the removed one is skipped");
        assert_eq!(s.completion().unwrap().cands, ["Photographs/", "Pictures old/"]);
        std::fs::create_dir(d.join("Plans")).unwrap();
        s.on_key(&tab(false), &mut tx);
        assert_eq!(s.cur().filter, format!("{base}Plans/"), "a new one joins in its place");
        std::fs::remove_dir(d.join("Photographs")).unwrap();
        std::fs::remove_dir(d.join("Pictures old")).unwrap();
        s.on_key(&tab(false), &mut tx);
        assert_eq!(s.cur().filter, format!("{base}Plans/"));
        assert!(s.completion().is_none(), "one left: completed whole, the card goes");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The first Tab completes the common prefix and lists the candidates; Tab again cycles
    /// them, ⇧Tab back; one candidate completes whole, a folder's with a separator; typing
    /// ends it.
    #[test]
    fn tab_completes_the_common_prefix_then_cycles() {
        let d = tmp("tab");
        std::fs::create_dir(d.join("report-2024")).unwrap();
        std::fs::write(d.join("report-2025.pdf"), "x").unwrap();
        std::fs::write(d.join("Readme"), "x").unwrap();
        let mut s = state_in(&d);
        settle_all(&mut s);
        let mut tx = Tx::default();
        let base = format!("{}/", d.display());
        type_path(&mut s, &format!("{base}rep"));
        assert!(s.on_key(&tab(false), &mut tx));
        assert_eq!(s.cur().filter, format!("{base}report-202"));
        let c = s.completion().expect("a card with the candidates");
        assert_eq!(c.cands, ["report-2024/", "report-2025.pdf"]);
        assert_eq!(c.at, None);
        s.on_key(&tab(false), &mut tx);
        assert_eq!(s.cur().filter, format!("{base}report-2024/"));
        s.on_key(&tab(false), &mut tx);
        assert_eq!(s.cur().filter, format!("{base}report-2025.pdf"));
        s.on_key(&tab(false), &mut tx);
        assert_eq!(s.cur().filter, format!("{base}report-2024/"), "it wraps");
        s.on_key(&tab(true), &mut tx);
        assert_eq!(s.cur().filter, format!("{base}report-2025.pdf"), "⇧Tab goes back");
        // Typing ends it: the card goes, and the next Tab completes what's typed now.
        s.cur_mut().type_filter("x");
        assert!(s.completion().is_none());
        // One candidate: all of it.
        type_path(&mut s, &format!("{base}Rea"));
        s.on_key(&tab(false), &mut tx);
        assert_eq!(s.cur().filter, format!("{base}Readme"));
        assert!(s.completion().is_none());
        type_path(&mut s, &format!("{base}report-2024"));
        s.on_key(&tab(false), &mut tx);
        assert_eq!(s.cur().filter, format!("{base}report-2024/"), "a folder gets a separator, ready for the next");
        // Nothing matches: the field stays, and the status bar says so.
        type_path(&mut s, &format!("{base}zzz"));
        s.on_key(&tab(false), &mut tx);
        assert_eq!(s.cur().filter, format!("{base}zzz"));
        assert!(s.note.as_deref().is_some_and(|n| n.contains("starts with “zzz”")), "{:?}", s.note);
        // Case rule: lowercase "rea" finds "Readme" only where capitals don't count.
        type_path(&mut s, &format!("{base}rea"));
        s.on_key(&tab(false), &mut tx);
        let want = if CASE_SENSITIVE { format!("{base}rea") } else { format!("{base}Readme") };
        assert_eq!(s.cur().filter, want);
    }

    /// Hidden items complete while hidden files are shown, or once a dot is typed.
    #[test]
    fn hidden_items_complete_when_shown_or_asked_for() {
        let d = tmp("hidden");
        std::fs::create_dir(d.join(".config")).unwrap();
        std::fs::write(d.join("code.rs"), "x").unwrap();
        let mut s = state_in(&d);
        settle_all(&mut s);
        let mut tx = Tx::default();
        let base = format!("{}/", d.display());
        type_path(&mut s, &base);
        s.on_key(&tab(false), &mut tx);
        assert_eq!(s.cur().filter, format!("{base}code.rs"), "only the visible one");
        type_path(&mut s, &format!("{base}."));
        s.on_key(&tab(false), &mut tx);
        assert_eq!(s.cur().filter, format!("{base}.config/"));
        s.toggle_hidden();
        type_path(&mut s, &base);
        s.on_key(&tab(false), &mut tx);
        assert_eq!(s.completion().map(|c| c.cands.len()), Some(2));
    }

    /// A folder that's slow to read: the key doesn't wait for it, and the answer applies when
    /// it lands, unless the field changed meanwhile.
    #[test]
    fn a_slow_folder_completes_when_it_lands() {
        let d = tmp("slow");
        std::fs::write(d.join("alpha"), "x").unwrap();
        let mut s = state_in(&d);
        settle_all(&mut s);
        s.show_wait = std::time::Duration::ZERO;
        let mut tx = Tx::default();
        let base = format!("{}/", d.display());
        type_path(&mut s, &format!("{base}al"));
        s.on_key(&tab(false), &mut tx);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while s.cur().filter.ends_with("/al") && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(2));
            s.poll(&mut tx);
        }
        assert_eq!(s.cur().filter, format!("{base}alpha"));
        // Typed on before it landed: the late answer is dropped.
        type_path(&mut s, &format!("{base}al"));
        s.on_key(&tab(false), &mut tx);
        s.cur_mut().type_filter("x");
        std::thread::sleep(std::time::Duration::from_millis(50));
        s.poll(&mut tx);
        assert_eq!(s.cur().filter, format!("{base}alx"));
    }

    /// A cycling Tab whose folder answers late: the answer applies when it lands, so a name
    /// removed since doesn't stay in the field.
    #[test]
    fn a_late_answer_while_cycling_applies_when_it_lands() {
        let d = std::env::temp_dir().join(format!("arbor-complete-late-cycle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for n in ["Photo", "Picture", "Plan"] {
            std::fs::create_dir_all(d.join(n)).unwrap();
        }
        let mut s = crate::app::tests::state_in(&d);
        crate::app::tests::settle_all(&mut s);
        let mut tx = Tx::default();
        s.show_wait = std::time::Duration::from_secs(5);
        s.cur_mut().filter = format!("{}/P", d.display());
        s.complete_tab(false, &mut tx);
        s.complete_tab(false, &mut tx);
        assert!(s.cur().filter.ends_with("/Photo/"), "{}", s.cur().filter);
        std::fs::remove_dir(d.join("Picture")).unwrap();
        // The folder answers after the key: the field shows what the last read said, then
        // what the folder holds now.
        s.show_wait = std::time::Duration::ZERO;
        s.complete_tab(false, &mut tx);
        assert!(s.cur().filter.ends_with("/Picture/"), "from the last read: {}", s.cur().filter);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !s.poll_completion() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(s.cur().filter.ends_with("/Plan/"), "the one after the removed name: {}", s.cur().filter);
        assert_eq!(s.completion().map(|c| c.cands.len()), Some(2));
        let _ = std::fs::remove_dir_all(&d);
    }
}
