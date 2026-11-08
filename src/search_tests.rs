use super::*;
use std::sync::mpsc::channel;

use crate::app::State;
use crate::app::tests::{settle_all, state_in};

fn tree(name: &str, files: &[&str]) -> PathBuf {
    let d = std::env::temp_dir().join(format!("arbor-search-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    for f in files {
        if let Some(dir) = f.strip_suffix('/') {
            std::fs::create_dir_all(d.join(dir)).unwrap();
        } else {
            if let Some(p) = Path::new(f).parent() {
                std::fs::create_dir_all(d.join(p)).unwrap();
            }
            std::fs::write(d.join(f), format!("contents of {f}")).unwrap();
        }
    }
    d
}

fn q(s: &str) -> Query {
    Query::parse(s).unwrap().unwrap()
}

// ---- matching -----------------------------------------------------------------------------

#[test]
fn a_word_is_a_case_insensitive_substring() {
    let m = q("read");
    assert!(m.matches_name("README.md") && m.matches_name("unread.txt") && m.matches_name("Readme"));
    assert!(!m.matches_name("rea.d"));
    assert!(q("ÉTÉ").matches_name("ÉTÉ.txt") && q("été").matches_name("ÉTÉ.txt"));
}

#[test]
fn a_capital_makes_a_word_case_sensitive() {
    let m = q("README");
    assert!(m.matches_name("README.md"));
    assert!(!m.matches_name("readme.md") && !m.matches_name("Readme.md"));
    // Per word: the other word still ignores case.
    let m = q("Cargo lock");
    assert!(m.matches_name("Cargo.LOCK") && !m.matches_name("cargo.lock"));
}

#[test]
fn star_question_mark_or_bracket_make_a_whole_name_glob() {
    let m = q("*.rs");
    assert!(m.matches_name("main.rs") && m.matches_name("MAIN.RS"));
    assert!(!m.matches_name("main.rsx") && !m.matches_name("rs.txt"));
    assert!(q("file?.txt").matches_name("file1.txt") && !q("file?.txt").matches_name("file10.txt"));
    assert!(q("[ab]*").matches_name("apple") && !q("[ab]*").matches_name("cherry"));
    // Smart case applies to globs too.
    assert!(q("*.RS").matches_name("x.RS") && !q("*.RS").matches_name("x.rs"));
    // `{…}` alone isn't a glob: it's searched for as typed.
    assert!(q("{a}").matches_name("x{a}y"));
}

#[test]
fn every_word_must_match() {
    let m = q("foo bar");
    assert!(m.matches_name("bar_foo.txt") && m.matches_name("foobar"));
    assert!(!m.matches_name("foo.txt") && !m.matches_name("bar.txt"));
    let m = q("*.jpg  2024");
    assert!(m.matches_name("IMG_2024_01.jpg") && !m.matches_name("IMG_2023.jpg") && !m.matches_name("2024.png"));
}

#[test]
fn an_empty_query_is_none_and_a_bad_glob_says_why() {
    assert!(Query::parse("   ").unwrap().is_none());
    let huge = format!("*{}", "{1,2,3,4}".repeat(5));
    assert!(Query::parse(&huge).is_err());
}

// ---- the walk -----------------------------------------------------------------------------

/// Everything a walk finds (paths relative to the root, sorted) and its last counts.
fn walk_all(root: &Path, query: &str, show_hidden: bool, cross_devices: bool, max: usize) -> (Vec<String>, Drained) {
    let pool = Pool::new(2);
    let spec = Spec { root: root.to_path_buf(), query: Arc::new(q(query)), show_hidden, cross_devices, max };
    let r = pool.start(spec, None);
    let mut found = Vec::new();
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let d = r.take();
        found.extend(d.batch.iter().map(|e| Path::new(&e.os).to_string_lossy().replace('\\', "/")));
        if d.done || Instant::now() > until {
            assert!(d.done, "never finished");
            found.sort();
            return (found, d);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_walk_finds_names_at_every_depth_with_their_relative_paths() {
    let d = tree("depth", &["notes.txt", "a/notes.md", "a/b/c/notes", "a/b/other", "notes-dir/", "x/y/"]);
    let (found, out) = walk_all(&d, "notes", false, false, MAX_RESULTS);
    assert_eq!(found, ["a/b/c/notes", "a/notes.md", "notes-dir", "notes.txt"]);
    assert_eq!(out.visited, 10, "every entry looked at once");
    assert_eq!((out.unreadable, out.capped, out.error), (0, false, None));
}

#[test]
fn results_carry_what_a_listing_shows() {
    let d = tree("meta", &["dir/file.txt", "dir/sub/"]);
    let pool = Pool::new(1);
    let r = pool.start(Spec { root: d.clone(), query: Arc::new(q("i")), show_hidden: false, cross_devices: false, max: 10 }, None);
    let until = Instant::now() + Duration::from_secs(10);
    let mut all = Vec::new();
    while Instant::now() < until {
        let x = r.take();
        all.extend(x.batch);
        if x.done {
            break;
        }
    }
    let file = all.iter().find(|e| e.name == "file.txt").expect("found the file");
    assert_eq!(Path::new(&file.os), Path::new("dir/file.txt"));
    assert_eq!((file.is_dir, file.size, file.lower.as_str()), (false, "contents of dir/file.txt".len() as u64, "file.txt"));
    assert!(file.mtime_ms > 0 && file.key.starts_with('r') && !file.key.contains('/'));
    let dir = all.iter().find(|e| e.name == "dir").expect("folders are results too");
    assert!(dir.is_dir && dir.size == 0);
    let keys: HashSet<&str> = all.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(keys.len(), all.len(), "keys are unique");
}

#[cfg(unix)]
#[test]
fn symlinked_folders_are_found_but_never_entered() {
    let d = tree("loop", &["a/x.txt"]);
    std::os::unix::fs::symlink(&d, d.join("a/loop")).unwrap();
    std::os::unix::fs::symlink(d.join("a"), d.join("alias")).unwrap();
    let (found, _) = walk_all(&d, "x", false, false, MAX_RESULTS);
    assert_eq!(found, ["a/x.txt"], "no loop, no second path to the same file");
    let (found, _) = walk_all(&d, "loop", false, false, MAX_RESULTS);
    assert_eq!(found, ["a/loop"]);
}

#[cfg(unix)]
#[test]
fn unreadable_folders_are_skipped_and_counted() {
    use std::os::unix::fs::PermissionsExt;
    let d = tree("unreadable", &["open/hit.txt", "locked/hit.txt", "locked2/deeper/hit.txt"]);
    for l in ["locked", "locked2"] {
        std::fs::set_permissions(d.join(l), std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    // Root reads everything: nothing to check there.
    let root = std::fs::read_dir(d.join("locked")).is_ok();
    let (found, out) = walk_all(&d, "hit", false, false, MAX_RESULTS);
    for l in ["locked", "locked2"] {
        std::fs::set_permissions(d.join(l), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    if root {
        return;
    }
    assert_eq!(found, ["open/hit.txt"]);
    assert_eq!(out.unreadable, 2);
    let mut f = Folder::new(9, d.clone(), crate::fs::Sort { key: crate::fs::SortKey::Name, desc: false }, false, None);
    let mut s = Search::new(f.memo(), "hit", None);
    (s.query, s.unreadable, s.took) = ("hit".into(), 2, Some(Duration::from_millis(1800)));
    f.search = Some(Box::new(s));
    let line = State::search_status(&f).unwrap();
    assert!(line.ends_with("· 1.8 s · 2 folders couldn't be read"), "{line}");
}

#[test]
fn an_unreadable_root_says_so() {
    let d = std::env::temp_dir().join(format!("arbor-search-missing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let (found, out) = walk_all(&d, "x", false, false, MAX_RESULTS);
    assert!(found.is_empty() && out.error.is_some());
}

/// `/dev` holds `ptmx` itself, and `/dev/pts` (another file system) holds another.
#[cfg(target_os = "linux")]
#[test]
fn other_file_systems_are_searched_only_when_asked() {
    use std::os::unix::fs::MetadataExt;
    let (dev, pts) = (std::fs::metadata("/dev"), std::fs::metadata("/dev/pts"));
    let ready = matches!((&dev, &pts), (Ok(a), Ok(b)) if a.dev() != b.dev()) && Path::new("/dev/pts/ptmx").exists();
    if !ready {
        return;
    }
    let (found, _) = walk_all(Path::new("/dev"), "ptmx", false, false, MAX_RESULTS);
    assert!(found.contains(&"ptmx".to_string()) && !found.contains(&"pts/ptmx".to_string()), "{found:?}");
    let (found, _) = walk_all(Path::new("/dev"), "ptmx", false, true, MAX_RESULTS);
    assert!(found.contains(&"pts/ptmx".to_string()), "{found:?}");
}

#[test]
fn pseudo_file_systems_are_skipped_from_the_root() {
    let pool = Pool::new(1);
    let r = pool
        .start(Spec { root: PathBuf::from("/"), query: Arc::new(q("zzzz-none")), show_hidden: false, cross_devices: true, max: 1 }, None);
    if cfg!(target_os = "linux") {
        assert_eq!(r.walk.skip, ["/proc", "/sys", "/dev"].map(PathBuf::from));
    }
    drop(r);
    let r = pool
        .start(Spec { root: PathBuf::from("/proc/self"), query: Arc::new(q("x")), show_hidden: false, cross_devices: true, max: 1 }, None);
    assert!(!r.walk.skip.iter().any(|p| p == Path::new("/proc")), "searching inside one searches it");
}

#[test]
fn hidden_items_and_everything_in_hidden_folders_follow_the_toggle() {
    let d = tree("hidden", &["hit.txt", ".hit-dot", ".cache/hit-inside", "seen/.hit-deep", "seen/hit"]);
    let (found, _) = walk_all(&d, "hit", false, false, MAX_RESULTS);
    assert_eq!(found, ["hit.txt", "seen/hit"]);
    let (found, _) = walk_all(&d, "hit", true, false, MAX_RESULTS);
    assert_eq!(found, [".cache/hit-inside", ".hit-dot", "hit.txt", "seen/.hit-deep", "seen/hit"]);
}

#[test]
fn a_walk_stops_at_the_result_cap() {
    let names: Vec<String> = (0..40).map(|i| format!("d{}/hit{i}", i % 4)).collect();
    let d = tree("cap", &names.iter().map(String::as_str).collect::<Vec<_>>());
    let (found, out) = walk_all(&d, "hit", false, false, 7);
    assert_eq!((found.len(), out.found, out.capped), (7, 7, true));
    let mut f = Folder::new(9, d.clone(), crate::fs::Sort { key: crate::fs::SortKey::Name, desc: false }, false, None);
    let mut s = Search::new(f.memo(), "hit", None);
    (s.query, s.capped, s.took) = ("hit".into(), true, Some(Duration::from_millis(300)));
    f.search = Some(Box::new(s));
    assert!(State::search_status(&f).unwrap().contains("stopped at 100,000 results"));
}

/// Holds `walk`'s read of `dir` at its first entry. Returns (started, go, left).
fn gate(dir: &Path) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<usize>) {
    let ((started, wait_start), (go_on, go), (left, wait_left)) = (channel(), channel(), channel());
    hook::GATES.lock().unwrap().push(hook::Gate { dir: dir.to_path_buf(), started, go, left });
    (wait_start, go_on, wait_left)
}

#[test]
fn a_stopped_walk_looks_at_nothing_more() {
    let names: Vec<String> = (0..50).map(|i| format!("sub{}/f{i}", i % 5)).collect();
    let d = tree("stop", &names.iter().map(String::as_str).collect::<Vec<_>>());
    let pool = Pool::new(3);
    let (started, go, left) = gate(&d);
    let r = pool.start(Spec { root: d.clone(), query: Arc::new(q("f")), show_hidden: false, cross_devices: false, max: 100 }, None);
    started.recv_timeout(Duration::from_secs(5)).expect("the walk reached the root's first entry");
    r.stop();
    go.send(()).unwrap();
    let after = left.recv_timeout(Duration::from_secs(5)).expect("it left the root");
    assert_eq!(after, 0, "not one more entry after the stop");
    let until = Instant::now() + Duration::from_secs(5);
    let mut d = r.take();
    while !d.done && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
        d = r.take();
    }
    assert!(d.done);
    assert_eq!((d.visited, d.found), (0, 0), "no folder below it was read");
}

#[test]
fn the_window_takes_at_most_one_batch_per_frame_and_is_woken_once_for_it() {
    let names: Vec<String> = (0..600).map(|i| format!("d{}/e{}/hit{i}", i % 12, i % 5)).collect();
    let d = tree("batch", &names.iter().map(String::as_str).collect::<Vec<_>>());
    let wakes = Arc::new(AtomicUsize::new(0));
    let w = wakes.clone();
    let waker: rune::Waker = Arc::new(move || {
        w.fetch_add(1, Relaxed);
    });
    let pool = Pool::new(4);
    let r =
        pool.start(Spec { root: d.clone(), query: Arc::new(q("hit")), show_hidden: false, cross_devices: false, max: 10_000 }, Some(waker));
    let (mut frames, mut total, mut biggest, mut seen) = (0, 0, 0, 0);
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        // A frame: what woke it, then one take.
        std::thread::sleep(Duration::from_millis(2));
        let woke = wakes.load(Relaxed);
        assert!(woke - seen <= 1, "woken {} times for one batch", woke - seen);
        seen = woke;
        let b = r.take();
        frames += 1;
        total += b.batch.len();
        biggest = biggest.max(b.batch.len());
        if b.done || Instant::now() > until {
            break;
        }
    }
    assert_eq!(total, 600);
    assert!(frames < 600 && biggest > 5, "found in batches, not one by one: {frames} frames, largest {biggest}");
}

// ---- a search in the window -----------------------------------------------------------------

fn tx() -> Tx {
    Tx::default()
}

/// Types `query` into the active pane's search (opening it first).
fn search(s: &mut State, query: &str) {
    if !s.cur().searching() {
        s.run(Cmd::Search, &mut tx());
    }
    let id = s.cur().id;
    s.cur_mut().search.as_mut().unwrap().edit.text = query.into();
    s.search_changed(id, &mut tx());
}

/// Polls as frames would until the active pane's walk is done.
fn finish_walk(s: &mut State) {
    let until = Instant::now() + Duration::from_secs(20);
    while s.cur().search.as_ref().is_some_and(|x| x.walking()) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
        s.poll(&mut tx());
    }
    assert!(!s.cur().search.as_ref().is_some_and(|x| x.walking()), "never finished");
}

/// The results' paths relative to the root, in order.
fn results(s: &State) -> Vec<String> {
    let f = s.cur();
    f.visible.iter().map(|&i| Path::new(&f.entries[i as usize].os).to_string_lossy().replace('\\', "/")).collect()
}

fn pick(s: &mut State, rel: &str) {
    let i = results(s).iter().position(|r| r == rel).unwrap_or_else(|| panic!("{rel} not in {:?}", results(s)));
    s.cur_mut().select(i);
}

/// Polls until the file operation is done and every pane has settled.
fn finish_job(s: &mut State) {
    let until = Instant::now() + Duration::from_secs(10);
    while s.job.is_some() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(2));
        s.poll(&mut tx());
    }
    assert!(s.job.is_none(), "the operation never finished");
    settle_all(s);
}

/// Polls a few frames, for a recheck to land.
fn frames(s: &mut State, n: usize) {
    for _ in 0..n {
        std::thread::sleep(Duration::from_millis(5));
        s.poll(&mut tx());
    }
}

#[test]
fn search_streams_results_and_reports_counts() {
    let d = tree("ui", &["a.txt", "sub/b.txt", "sub/deeper/c.txt", "sub/other.md"]);
    let mut s = state_in(&d);
    assert!(s.keymap.shortcut(Cmd::Search).is_some());
    s.run(Cmd::Search, &mut tx());
    assert!(s.cur().searching() && s.cur().entries.is_empty(), "an empty search view, at once");
    assert_eq!(s.cur().cwd, d);
    assert!(State::search_status(s.cur()).unwrap().starts_with("Search names in"));
    search(&mut s, ".txt");
    assert!(State::search_status(s.cur()).unwrap().starts_with("Searching… "));
    finish_walk(&mut s);
    let mut r = results(&s);
    r.sort();
    assert_eq!(r, ["a.txt", "sub/b.txt", "sub/deeper/c.txt"]);
    let line = State::search_status(s.cur()).unwrap();
    assert!(line.starts_with("3 found in 6 items · ") && (line.ends_with(" ms") || line.ends_with(" s")), "{line}");
    // Nothing is kept: the same search walks again.
    let g = s.cur().search.as_ref().unwrap().generation;
    s.run(Cmd::Reload, &mut tx());
    assert_eq!(s.cur().search.as_ref().unwrap().generation, g + 1);
    finish_walk(&mut s);
    assert_eq!(results(&s).len(), 3);
}

#[test]
fn a_query_change_stops_the_walk_at_once() {
    let names: Vec<String> = (0..60).map(|i| format!("s{}/f{i}", i % 6)).collect();
    let d = tree("ui-cancel", &names.iter().map(String::as_str).collect::<Vec<_>>());
    let mut s = state_in(&d);
    let (started, go, left) = gate(&d);
    search(&mut s, "f");
    started.recv_timeout(Duration::from_secs(5)).expect("the walk started");
    search(&mut s, "f1");
    go.send(()).unwrap();
    assert_eq!(left.recv_timeout(Duration::from_secs(5)).expect("the old walk left the root"), 0, "it looked at nothing more");
    finish_walk(&mut s);
    let mut r = results(&s);
    r.sort();
    assert_eq!(r.len(), 11, "the new query's results only: f1, f10…f19 {r:?}");
}

#[test]
fn hiding_or_leaving_the_tab_stops_the_walk() {
    let names: Vec<String> = (0..60).map(|i| format!("s{}/f{i}", i % 6)).collect();
    let d = tree("ui-hide", &names.iter().map(String::as_str).collect::<Vec<_>>());
    let other = tree("ui-hide-other", &["x"]);
    let mut s = state_in(&d);
    s.show_wait = Duration::ZERO;
    s.run(Cmd::NewTab, &mut tx());
    s.go_to(other.clone());
    settle_all(&mut s);
    s.run(Cmd::PrevTab, &mut tx());
    settle_all(&mut s);
    // Hidden: the next tab is shown.
    let (started, go, left) = gate(&d);
    search(&mut s, "f");
    started.recv_timeout(Duration::from_secs(5)).expect("the walk started");
    s.run(Cmd::NextTab, &mut tx());
    go.send(()).unwrap();
    assert_eq!(left.recv_timeout(Duration::from_secs(5)).unwrap(), 0);
    // Shown again: it searches again, for the same query.
    s.run(Cmd::PrevTab, &mut tx());
    assert!(s.cur().searching());
    assert_eq!(s.cur().search.as_ref().unwrap().query, "f");
    finish_walk(&mut s);
    assert_eq!(results(&s).len(), 60);
    // Esc while walking, navigating away, and closing the tab stop it too.
    for how in ["esc", "navigate", "close"] {
        let (started, go, left) = gate(&d);
        s.run(Cmd::Reload, &mut tx());
        started.recv_timeout(Duration::from_secs(5)).expect("the walk started");
        match how {
            "esc" => s.run(Cmd::Clear, &mut tx()),
            "navigate" => s.go_to(other.clone()),
            _ => s.run(Cmd::CloseTab, &mut tx()),
        }
        go.send(()).unwrap();
        assert_eq!(left.recv_timeout(Duration::from_secs(5)).unwrap(), 0, "{how}");
        if how == "esc" {
            assert!(s.cur().searching() && !s.cur().search.as_ref().unwrap().walking(), "stopped, still showing what it found");
            assert!(State::search_status(s.cur()).unwrap().starts_with("Stopped · "));
        }
        if how == "navigate" {
            settle_all(&mut s);
            assert!(!s.cur().searching());
            s.run(Cmd::Back, &mut tx());
            settle_all(&mut s);
            s.run(Cmd::Search, &mut tx());
            search(&mut s, "f");
            finish_walk(&mut s);
        }
    }
}

#[test]
fn leaving_the_search_restores_the_listing() {
    let d = tree("ui-leave", &["apple", "banana", "cherry", "sub/banana-split"]);
    let mut s = state_in(&d);
    s.cur_mut().select(2); // "banana" (folders first: sub, apple, banana)
    let keep = s.cur().selected().unwrap().name.clone();
    s.cur_mut().type_filter("an");
    let filtered = s.cur().visible.len();
    search(&mut s, "banana");
    finish_walk(&mut s);
    assert_eq!(results(&s).len(), 2);
    assert!(s.cur().filter.is_empty(), "the filter is put aside while searching");
    // Esc with the walk done leaves.
    s.run(Cmd::Clear, &mut tx());
    settle_all(&mut s);
    assert!(!s.cur().searching());
    assert_eq!((s.cur().filter.as_str(), s.cur().visible.len()), ("an", filtered));
    assert_eq!(s.cur().selected().unwrap().name, keep);
    // So do Back, and clearing the query.
    search(&mut s, "cherry");
    s.run(Cmd::Back, &mut tx());
    settle_all(&mut s);
    assert!(!s.cur().searching() && s.cur().cwd == d);
    search(&mut s, "cherry");
    search(&mut s, "");
    settle_all(&mut s);
    assert!(!s.cur().searching());
    assert_eq!(s.cur().selected().unwrap().name, keep);
}

#[test]
fn typing_with_the_results_focused_edits_the_query() {
    let d = tree("ui-type", &["alpha", "beta", "sub/alphabet"]);
    let mut s = state_in(&d);
    search(&mut s, "alp");
    finish_walk(&mut s);
    let id = s.cur().id;
    let enter = rune::KeyEvent { key: Key::Enter, mods: rune::Mods::default() };
    assert!(s.search_field_key(id, &enter, &mut tx()));
    assert!(!s.cur().search.as_ref().unwrap().edit.focused, "Enter hands the keys to the results");
    let key = |k: Key| rune::KeyEvent { key: k, mods: rune::Mods::default() };
    assert!(s.on_key(&key(Key::Text("h".into())), &mut tx()));
    assert_eq!(s.cur().search.as_ref().unwrap().query, "alph");
    assert!(s.cur().search.as_ref().unwrap().edit.focused);
    assert!(s.on_key(&key(Key::Backspace), &mut tx()));
    assert_eq!(s.cur().search.as_ref().unwrap().query, "alp");
    finish_walk(&mut s);
    // Space, with the results focused, previews.
    s.focus_results(0, &mut tx());
    assert_eq!(s.unavailable(Cmd::TogglePreview), None);
}

/// Two results with the same name in different folders: each action reaches the one selected.
#[test]
fn actions_reach_each_results_own_path() {
    let d = tree("ui-same", &["one/report.txt", "two/report.txt", "two/keep.txt"]);
    std::fs::write(d.join("two/report.txt"), "the second report").unwrap();
    let mut s = state_in(&d);
    search(&mut s, "report");
    finish_walk(&mut s);
    pick(&mut s, "two/report.txt");
    assert_eq!(s.target_paths(), [d.join("two/report.txt")]);
    assert_eq!(s.cur().drag_paths(s.cur().sel), [d.join("two/report.txt")]);
    // Trash it: the other one stays, and its row drops without the walk running again.
    let g = s.cur().search.as_ref().unwrap().generation;
    s.run(Cmd::Trash, &mut tx());
    finish_job(&mut s);
    assert!(!d.join("two/report.txt").exists() && d.join("one/report.txt").exists());
    assert!(!d.join("report.txt").exists());
    frames(&mut s, 40);
    assert_eq!(results(&s), ["one/report.txt"]);
    assert_eq!(s.cur().search.as_ref().unwrap().generation, g, "updated, not walked again");
    // Undo puts it back; it joins the results again (it landed inside the root and matches).
    s.run(Cmd::Undo, &mut tx());
    finish_job(&mut s);
    frames(&mut s, 40);
    let mut r = results(&s);
    r.sort();
    assert_eq!(r, ["one/report.txt", "two/report.txt"]);
    assert_eq!(std::fs::read_to_string(d.join("two/report.txt")).unwrap(), "the second report");
}

#[test]
fn renaming_a_result_renames_it_in_its_own_folder() {
    let d = tree("ui-rename", &["deep/er/target.txt", "target.txt.bak", "deep/er/inner/target-notes"]);
    let mut s = state_in(&d);
    search(&mut s, "target");
    finish_walk(&mut s);
    pick(&mut s, "deep/er/target.txt");
    s.start_rename();
    s.rename.as_mut().unwrap().edit.text = "renamed.txt".into();
    s.commit_rename(&mut tx());
    assert!(d.join("deep/er/renamed.txt").exists() && !d.join("renamed.txt").exists());
    assert!(!d.join("deep/er/target.txt").exists());
    assert_eq!(
        s.cur().selected().map(|e| (e.name.as_str(), PathBuf::from(&e.os))),
        Some(("renamed.txt", PathBuf::from("deep/er/renamed.txt")))
    );
    assert_eq!(s.target_paths(), [d.join("deep/er/renamed.txt")]);
    // A folder: the results inside it follow it.
    search(&mut s, "e");
    finish_walk(&mut s);
    pick(&mut s, "deep/er");
    s.start_rename();
    s.rename.as_mut().unwrap().edit.text = "moved".into();
    s.commit_rename(&mut tx());
    let r = results(&s);
    assert!(r.contains(&"deep/moved".into()) && r.contains(&"deep/moved/renamed.txt".into()), "{r:?}");
    assert!(!r.iter().any(|p| p.starts_with("deep/er")), "{r:?}");
    pick(&mut s, "deep/moved/renamed.txt");
    assert_eq!(s.target_paths(), [d.join("deep/moved/renamed.txt")]);
    assert!(s.target_paths()[0].exists());
}

#[test]
fn duplicate_copy_to_the_other_pane_and_open_use_full_paths() {
    let d = tree("ui-ops", &["a/x.txt", "b/x.txt", "b/inner/"]);
    let other = tree("ui-ops-other", &[]);
    let mut s = state_in(&d);
    s.run(Cmd::Split, &mut tx());
    s.go_to(other.clone());
    settle_all(&mut s);
    s.run(Cmd::OtherPane, &mut tx());
    search(&mut s, "x");
    finish_walk(&mut s);
    pick(&mut s, "b/x.txt");
    // Beside it, not in the root.
    s.run(Cmd::Duplicate, &mut tx());
    finish_job(&mut s);
    assert!(d.join("b/x copy.txt").exists() && !d.join("x copy.txt").exists());
    frames(&mut s, 40);
    assert!(results(&s).contains(&"b/x copy.txt".to_string()), "the copy joins the results: {:?}", results(&s));
    // Several folders at once can't be duplicated (where would they go?).
    s.cur_mut().select_all();
    assert_eq!(s.unavailable(Cmd::Duplicate), Some("Duplicate items from one folder at a time"));
    pick(&mut s, "a/x.txt");
    s.run(Cmd::CopyToOther, &mut tx());
    finish_job(&mut s);
    assert_eq!(std::fs::read_to_string(other.join("x.txt")).unwrap(), "contents of a/x.txt");
    // Nothing goes into a pane showing results, nor is pasted or made in the root.
    s.run(Cmd::OtherPane, &mut tx());
    assert_eq!(s.unavailable(Cmd::CopyToOther), Some(crate::app::IN_OTHER_SEARCH));
    s.run(Cmd::OtherPane, &mut tx());
    assert!(s.unavailable(Cmd::Paste).is_some() && s.unavailable(Cmd::NewFolder).is_some());
    // Opening a folder result goes there; Back comes back to the root's listing.
    search(&mut s, "inner");
    finish_walk(&mut s);
    pick(&mut s, "b/inner");
    s.run(Cmd::Open, &mut tx());
    settle_all(&mut s);
    assert!(!s.cur().searching());
    assert_eq!(s.cur().cwd, d.join("b/inner"));
    s.run(Cmd::Back, &mut tx());
    settle_all(&mut s);
    assert_eq!(s.cur().cwd, d);
}

#[test]
fn select_by_pattern_and_sorting_work_on_results() {
    let d = tree("ui-sort", &["z/big.log", "a/small.log", "m/mid.txt"]);
    std::fs::write(d.join("z/big.log"), "x".repeat(500)).unwrap();
    let mut s = state_in(&d);
    search(&mut s, ".");
    finish_walk(&mut s);
    assert!(!s.cur().search.as_ref().unwrap().sorted);
    let found = results(&s);
    s.set_sort(crate::fs::SortKey::Size);
    assert!(s.cur().search.as_ref().unwrap().sorted);
    assert_eq!(results(&s)[0], "z/big.log", "a header click sorts them, largest first");
    assert_eq!(results(&s).len(), found.len());
    let g = Glob::name("*.log", Case::Insensitive).unwrap();
    let matches = s.cur().matching(&g);
    assert_eq!(matches.len(), 2);
    s.cur_mut().apply_matches(matches, crate::select::How::Replace);
    let mut t = s.target_paths();
    t.sort();
    assert_eq!(t, [d.join("a/small.log"), d.join("z/big.log")]);
}

#[test]
fn a_split_or_new_tab_from_a_search_shows_the_folder() {
    let d = tree("ui-split", &["a/hit", "b"]);
    let mut s = state_in(&d);
    search(&mut s, "hit");
    finish_walk(&mut s);
    s.run(Cmd::NewTab, &mut tx());
    settle_all(&mut s);
    assert!(!s.cur().searching());
    assert_eq!(s.cur().entries.len(), 2, "the folder's own listing");
    assert_eq!(s.panes[0].tabs[0].as_ref().unwrap().search.as_deref(), Some("hit"));
    s.run(Cmd::PrevTab, &mut tx());
    finish_walk(&mut s);
    assert_eq!(results(&s), ["a/hit"]);
    s.run(Cmd::Split, &mut tx());
    settle_all(&mut s);
    assert!(!s.cur().searching() && s.cur().entries.len() == 2);
}

/// Time to the first result and to the end, by thread count, over `vendor/` (about 550 MB) or
/// `ARBOR_SEARCH_TREE`. Warm cache (no root here to drop it), median of 7.
/// `cargo test --release -- --ignored --nocapture search_speed`
#[test]
#[ignore = "benchmark: walks a large tree"]
fn search_speed() {
    let root =
        std::env::var_os("ARBOR_SEARCH_TREE").map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor"), PathBuf::from);
    for query in ["mod.rs", "zzqx-none"] {
        for threads in [1, 2, 4] {
            let pool = Pool::new(threads);
            let mut runs: Vec<(f64, f64, usize, u64)> = (0..7)
                .map(|_| {
                    let t = Instant::now();
                    let r = pool.start(
                        Spec { root: root.clone(), query: Arc::new(q(query)), show_hidden: false, cross_devices: false, max: MAX_RESULTS },
                        None,
                    );
                    let (mut first, mut found) = (None, 0);
                    loop {
                        let d = r.take();
                        if !d.batch.is_empty() && first.is_none() {
                            first = Some(t.elapsed().as_secs_f64() * 1000.0);
                        }
                        found += d.batch.len();
                        if d.done {
                            return (first.unwrap_or(f64::NAN), t.elapsed().as_secs_f64() * 1000.0, found, d.visited);
                        }
                        std::thread::sleep(Duration::from_micros(200));
                    }
                })
                .collect();
            runs.sort_by(|a, b| a.1.total_cmp(&b.1));
            let (first, total, found, visited) = runs[3];
            eprintln!("{query:>10} threads {threads}: first result {first:6.2} ms, total {total:7.1} ms, {found} found in {visited} items");
        }
    }
}

/// While a walk goes on the list grows only to `LIVE_ROWS` (a row count change makes Rune read
/// every key again); the rest join when it ends, or when Esc stops it.
#[test]
fn the_list_grows_only_so_far_while_the_walk_goes_on() {
    let d = tree("live", &["seed"]);
    let mut s = state_in(&d);
    search(&mut s, "hit");
    let f = s.cur_mut();
    let fake = |i: usize| Entry {
        os: format!("x/hit{i}").into(),
        name: format!("hit{i}"),
        lower: format!("hit{i}"),
        key: next_key(),
        is_dir: false,
        is_link: false,
        hidden: false,
        size: 1,
        mtime_ms: 1,
    };
    // As poll_search appends batches while the walk is still going.
    for batch in 0..3 {
        f.entries.extend((0..700).map(|i| fake(batch * 700 + i)));
        f.take_in();
    }
    assert_eq!((f.entries.len(), f.visible.len()), (2100, LIVE_ROWS));
    let line = State::search_status(f).unwrap();
    assert!(line.contains("2,100 found") && line.ends_with("showing the first 1,000 until it's done"), "{line}");
    // Selection works on what's shown.
    f.select_all();
    assert_eq!(f.marked.len(), LIVE_ROWS);
    // Esc stops it: everything found joins the list.
    s.stop_search(0);
    let f = s.cur();
    assert!(f.entries.len() >= 2100 && f.visible.len() == f.entries.len(), "{} of {}", f.visible.len(), f.entries.len());
    assert!(!State::search_status(f).unwrap().contains("showing the first"));
}

/// Batch Rename on search results: each item is renamed in its own folder, two of one name in
/// different folders are no clash, and the results show the new names (and stay selected).
#[test]
fn batch_rename_renames_results_in_their_own_folders() {
    let d = tree("batch-rename", &["one/report.txt", "two/report.txt", "two/report-old.txt", "x.md"]);
    let mut s = state_in(&d);
    search(&mut s, "report");
    finish_walk(&mut s);
    s.run(Cmd::SelectAll, &mut tx());
    s.run(Cmd::Rename, &mut tx());
    let b = s.batch.as_mut().expect("Batch Rename on the results");
    b.find = rune::TextEdit::new("report");
    b.replace = rune::TextEdit::new("{parent}");
    s.batch_replan();
    let b = s.batch.as_ref().unwrap();
    assert!(b.known());
    let mut planned: Vec<(String, String)> = b.items.iter().map(|i| i.name.clone()).zip(b.plan.new.iter().cloned()).collect();
    planned.sort();
    assert_eq!(
        planned,
        [("report-old.txt".into(), "two-old.txt".into()), ("report.txt".into(), "one.txt".into()), ("report.txt".into(), "two.txt".into())]
    );
    assert_eq!(b.blocked(), None);
    s.batch_apply(&mut tx());
    finish_job(&mut s);
    for (path, was) in [("one/one.txt", "one/report.txt"), ("two/two.txt", "two/report.txt"), ("two/two-old.txt", "two/report-old.txt")] {
        assert_eq!(std::fs::read_to_string(d.join(path)).unwrap(), format!("contents of {was}"));
    }
    frames(&mut s, 5);
    let mut shown = results(&s);
    shown.sort();
    assert_eq!(shown, ["one/one.txt", "two/two-old.txt", "two/two.txt"]);
    assert_eq!(s.cur().marked.len(), 3, "still selected, under their new names");
    // Onto another item of the batch, which keeps its name: a duplicate.
    let rule = |s: &mut State| {
        let b = s.batch.as_mut().unwrap();
        b.find = rune::TextEdit::new("two-old");
        b.replace = rune::TextEdit::new("two");
        s.batch_replan();
        let b = s.batch.as_ref().unwrap();
        let i = b.items.iter().position(|i| i.name == "two-old.txt").unwrap();
        b.plan.status[i].clone()
    };
    s.run(Cmd::Rename, &mut tx());
    assert_eq!(rule(&mut s), crate::rename::Status::Problem("Same new name as “two.txt”".into()));
    // Onto a name in the item's own folder, outside the batch: already there.
    s.close_batch(&mut tx());
    pick(&mut s, "two/two-old.txt");
    s.run(Cmd::BatchRename, &mut tx());
    assert_eq!(rule(&mut s), crate::rename::Status::Problem("“two.txt” already exists here".into()));
}

/// Starting a walk touches no disk on the window's thread: the root's device is looked up by
/// the thread that reads it. Here that lookup is held (as a sleeping disk would hold it): the
/// key that started the search returns at once, and the walk goes on when it's let go.
#[test]
fn starting_a_search_never_waits_for_the_disk() {
    let d = tree("no-block", &["a.txt", "sub/b.txt"]);
    let mut s = state_in(&d);
    let (go, gate) = channel();
    hook::DEVICES.lock().unwrap().push((d.clone(), gate));
    let t = Instant::now();
    search(&mut s, "txt");
    assert!(t.elapsed() < Duration::from_millis(1000), "the key waited {:?} for the disk", t.elapsed());
    go.send(()).unwrap();
    finish_walk(&mut s);
    let mut r = results(&s);
    r.sort();
    assert_eq!(r, ["a.txt", "sub/b.txt"]);
}

/// Select by Pattern's count follows results that stream in while it's open: they're appended
/// without a new listing version, and the count used to stay at what it first saw.
#[test]
fn select_by_pattern_counts_results_that_stream_in() {
    let d = tree("pick-stream", &["hit-root", "sub/hit-child"]);
    let mut s = state_in(&d);
    let (started, go, left) = gate(&d.join("sub"));
    search(&mut s, "hit");
    started.recv_timeout(Duration::from_secs(5)).unwrap();
    frames(&mut s, 3);
    s.open_pick(&mut tx());
    s.pick_edit().insert("*");
    s.pick_refresh();
    assert_eq!(s.pick.as_ref().unwrap().count, Ok((1, 1)));
    go.send(()).unwrap();
    left.recv_timeout(Duration::from_secs(5)).unwrap();
    finish_walk(&mut s);
    assert_eq!(s.cur().visible.len(), 2);
    assert_eq!(s.pick.as_ref().unwrap().count, Ok((2, 2)), "counted again when results arrived");
}

/// A recheck after a file operation stops where it is when the query changes (or the search
/// ends, or a newer recheck replaces it): it used to go on looking at every result it had.
#[test]
fn a_recheck_stops_when_the_query_changes() {
    let names: Vec<String> = (0..30).map(|i| format!("hit-{i:02}")).collect();
    let d = tree("recheck-stop", &names.iter().map(String::as_str).collect::<Vec<_>>());
    let mut s = state_in(&d);
    search(&mut s, "hit");
    finish_walk(&mut s);
    assert_eq!(results(&s).len(), 30);
    let (started, started_rx) = channel();
    let (go, go_rx) = channel();
    let after = Arc::new(AtomicUsize::new(0));
    hook::RECHECK.lock().unwrap().push(hook::RecheckGate { dir: d.clone(), started, go: Some(go_rx), after: after.clone() });
    // As a finished operation in the root does.
    s.refresh(&[(d.clone(), None)], &[]);
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    search(&mut s, "other");
    go.send(()).unwrap();
    frames(&mut s, 10);
    hook::RECHECK.lock().unwrap().retain(|g| g.dir != d);
    assert_eq!(after.load(Relaxed), 0, "looked at results after the query changed");
    // Leaving the search stops one too.
    search(&mut s, "hit");
    finish_walk(&mut s);
    let (started, started_rx) = channel();
    let (go, go_rx) = channel();
    let after = Arc::new(AtomicUsize::new(0));
    hook::RECHECK.lock().unwrap().push(hook::RecheckGate { dir: d.clone(), started, go: Some(go_rx), after: after.clone() });
    s.refresh(&[(d.clone(), None)], &[]);
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    s.leave_search(s.active, &mut tx());
    go.send(()).unwrap();
    frames(&mut s, 10);
    hook::RECHECK.lock().unwrap().retain(|g| g.dir != d);
    assert_eq!(after.load(Relaxed), 0, "looked at results after the search ended");
    // And the close hook stops walks and rechecks.
    search(&mut s, "hit");
    s.stop_background();
    assert!(!s.cur().search.as_ref().unwrap().walking());
}
