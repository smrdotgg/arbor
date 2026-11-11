use super::*;

fn p(s: &str) -> PathBuf {
    if cfg!(windows) { PathBuf::from(format!("C:{}", s.replace('/', "\\"))) } else { PathBuf::from(s) }
}

fn visit(f: &mut Frecency, s: &str, at: u64, n: usize) {
    for _ in 0..n {
        f.apply(&Event::Visit { path: p(s), at, rank: 1.0 });
    }
}

fn top(f: &Frecency, q: &str, now: u64) -> Vec<PathBuf> {
    f.rank(q, now, 10).into_iter().map(|h| h.path.to_path_buf()).collect()
}

#[test]
fn frecency_weighs_recent_visits_more() {
    let now = 10 * WEEK;
    let d = |last: u64| Dir::new(p("/x").into(), 2.0, last).frecency(now);
    assert_eq!(d(now - 10), 8.0);
    assert_eq!(d(now - 2 * HOUR), 4.0);
    assert_eq!(d(now - 2 * DAY), 1.0);
    assert_eq!(d(now - 2 * WEEK), 0.5);
}

#[test]
fn words_match_in_order_and_the_last_one_names_the_folder() {
    let now = 10 * WEEK;
    let mut f = Frecency::default();
    visit(&mut f, "/home/u/projects/arbor", now - 100, 3);
    visit(&mut f, "/home/u/projects/arbor/src", now - 100, 10);
    visit(&mut f, "/home/u/archive/old-projects", now - 100, 5);
    visit(&mut f, "/home/u/projects/rune", now - 100, 1);
    // "proj arb": arbor first, though src is visited more (its name doesn't match "arb").
    assert_eq!(top(&f, "proj arb", now)[0], p("/home/u/projects/arbor"));
    // Order matters: "arb proj" needs arb before proj in the path.
    assert!(top(&f, "arb proj", now).is_empty());
    // A single word: the folder named that, then the deeper one that matches only a parent.
    assert_eq!(top(&f, "arbor", now), [p("/home/u/projects/arbor"), p("/home/u/projects/arbor/src")]);
    // Case doesn't matter.
    assert_eq!(top(&f, "RUNE", now), [p("/home/u/projects/rune")]);
    // Nothing typed: by frecency.
    assert_eq!(top(&f, "", now)[0], p("/home/u/projects/arbor/src"));
    // Frecency still counts among good matches: old-projects (5 visits) ranks above
    // projects/arbor/src for "proj"? No: src's last word doesn't name "proj"; both lose
    // to folders whose own name has it.
    assert_eq!(top(&f, "proj", now)[0], p("/home/u/archive/old-projects"));
}

#[test]
fn a_longer_word_or_another_word_narrows_a_query() {
    assert!(narrows("", "a") && narrows("pro", "proj") && narrows("proj", "proj arb") && narrows("proj a", "proj arb"));
    assert!(narrows("proj  arb", "proj arb ") && narrows("proj", "proj"));
    assert!(!narrows("proj", "pro") && !narrows("proj arb", "proj") && !narrows("proj arb", "proj xarb") && !narrows("ab", "ba"));
}

/// Ranking among the last query's matches gives what ranking everything gives, key after
/// key, and a history that changed in between is ranked whole again.
#[test]
fn typing_on_ranks_among_the_last_matches_and_gets_the_same_rows() {
    let now = 10 * WEEK;
    let mut f = Frecency::default();
    let words = ["projects", "arbor", "src", "photos", "2024", "work"];
    for i in 0..600u64 {
        let path = format!("/h/{}/{}-{i}/{}", words[(i % 6) as usize], words[(i / 6 % 6) as usize], words[(i / 36 % 6) as usize]);
        visit(&mut f, &path, now - i * 600, 1 + (i % 5) as usize);
    }
    let (mut last, mut matched): (String, Vec<u32>) = (String::new(), (0..f.len() as u32).collect());
    for q in ["p", "pr", "pro", "proj", "proj a", "proj ar", "proj arb", "proj arb s"] {
        assert!(narrows(&last, q));
        let (hits, m) = f.rank_in(q, now, LIMIT, Some(&matched));
        let whole = f.rank(q, now, LIMIT);
        assert_eq!(hits, whole, "{q:?}");
        (last, matched) = (q.to_string(), m);
    }
    // The box: a visit lands between two keys, so the next key ranks everything again.
    let mut j = (String::from("proj"), f.generation());
    visit(&mut f, "/h/projects/new-one/arbor", now, 50);
    assert_ne!(j.1, f.generation());
    j.0.push_str(" arb");
    assert_eq!(f.rank("proj arb", now, 1)[0].path.as_ref(), p("/h/projects/new-one/arbor").as_path());
}

#[test]
fn matched_characters_are_marked_in_the_shown_path() {
    let now = 1000;
    let mut f = Frecency::default();
    visit(&mut f, "/srv/Projects/Arbor", now, 1);
    let h = &f.rank("proj arb", now, 1)[0];
    let marked: Vec<&str> = h.marks(&words("proj arb")).iter().map(|&(a, b)| &h.shown[a..b]).collect();
    assert_eq!(marked, ["Proj", "Arb"]);
}

#[test]
fn the_log_aggregates_and_skips_a_cut_short_last_line() {
    let a = p("/a").display().to_string();
    let b = p("/b").display().to_string();
    let text = format!("100\t1\t{a}\n200\t1\t{a}\nnot a line\n150\t2.5\t{b}\n300\t1\t{b}");
    let (f, lines) = Frecency::from_log(&text);
    assert_eq!(lines, 4);
    assert_eq!(f.get(&p("/a")).map(|d| (d.rank, d.last)), Some((2.0, 200)));
    assert_eq!(f.get(&p("/b")).map(|d| (d.rank, d.last)), Some((2.5, 150)), "the cut-short visit isn't counted");
    // A drop removes it; a visit after brings it back.
    let (f, _) = Frecency::from_log(&format!("{text}\n400\t-\t{b}\n"));
    assert!(f.get(&p("/b")).is_none());
    let (f, _) = Frecency::from_log(&format!("{text}\n400\t-\t{b}\n500\t1\t{b}\n"));
    assert_eq!(f.get(&p("/b")).map(|d| d.rank), Some(1.0));
    // Garbage, relative paths and bad ranks are skipped.
    let (f, _) = Frecency::from_log("x\ty\tz\n1\t1\trelative\n1\tNaN\t/n\n1\t-3\t/n\n\n");
    assert!(f.is_empty());
}

#[test]
fn ranks_age_so_old_habits_fade() {
    let mut f = Frecency::default();
    visit(&mut f, "/old", 1, 9_000);
    visit(&mut f, "/rare", 1, 1);
    visit(&mut f, "/new", 2, 1_500);
    let total: f64 = f.dirs.iter().map(|d| d.rank).sum();
    assert!(total <= MAX_AGE, "{total}");
    assert!(f.get(&p("/rare")).is_none(), "under rank 1 after aging");
    assert!(f.get(&p("/old")).is_some() && f.get(&p("/new")).is_some());
}

#[test]
fn compaction_keeps_the_top_folders_as_one_line_each() {
    let now = 10 * WEEK;
    let mut f = Frecency::default();
    for i in 0..(KEEP + 50) {
        visit(&mut f, &format!("/d/{i}"), now - i as u64, 1);
    }
    visit(&mut f, "/d/7", now, 1);
    let text = f.compacted(now);
    let (g, lines) = Frecency::from_log(&text);
    assert_eq!((lines, g.len()), (KEEP, KEEP));
    assert_eq!(g.get(&p("/d/7")).map(|d| (d.rank, d.last)), Some((2.0, now)));
    assert!(g.get(&p("/d/0")).is_some() && g.get(&p(&format!("/d/{}", KEEP + 49))).is_none(), "the oldest go");
}

#[test]
fn exclude_globs_keep_folders_out() {
    let mut h = History::new(None);
    h.agg = Some(Frecency::default());
    let tmp = p("/tmp").display().to_string();
    h.exclude = exclude_globs(&[format!("{tmp}/**"), "**/node_modules".into()]);
    h.record(&p("/tmp/x/y"));
    h.record(&p("/tmp"));
    h.record(&p("/w/node_modules"));
    h.record(&p("/w/src"));
    let a = h.agg.as_ref().unwrap();
    assert_eq!(a.len(), 1);
    assert!(a.get(&p("/w/src")).is_some());
    h.enabled = false;
    h.record(&p("/w/other"));
    assert_eq!(h.agg.as_ref().unwrap().len(), 1, "off records nothing");
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("arbor-jump-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.join("history.log")
}

/// Waits for the writer to have done `n` disk operations. It counts one as it starts it,
/// so the flush then waits for the last one to finish (on a fast machine a test could
/// otherwise read the log before the append landed).
fn settle_io(h: &History, n: u64) {
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while h.io() < n && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(h.io(), n);
    assert!(h.drain(Duration::from_secs(5)), "the writer finishes what it started");
}

#[test]
fn visits_append_to_the_log_and_load_back() {
    let log = scratch("append");
    // A cut-short last line from an earlier crash: the next append ends it so it stays
    // invalid, and starts a new line.
    std::fs::write(&log, format!("5\t1\t{}\n6\t1\t{}", p("/a").display(), p("/cut").display())).unwrap();
    let mut h = History::new(Some(log.clone()));
    h.record(&p("/b"));
    h.record(&p("/b"));
    settle_io(&h, 2);
    let text = std::fs::read_to_string(&log).unwrap();
    assert_eq!(text.lines().count(), 4, "{text}");
    assert!(text.ends_with('\n'));
    assert!(h.load(Duration::from_secs(5), None));
    let a = h.agg.as_ref().unwrap();
    assert_eq!(a.get(&p("/b")).map(|d| d.rank), Some(2.0));
    assert!(a.get(&p("/cut")).is_none(), "a line cut short never comes back as a visit");
    assert!(a.get(&p("/a")).is_some());
    // Forgetting records a drop, which the next session's load honours.
    h.forget(&p("/b"));
    settle_io(&h, 4);
    let mut again = History::new(Some(log.clone()));
    assert!(again.load(Duration::from_secs(5), None));
    assert!(again.agg.as_ref().unwrap().get(&p("/b")).is_none());
    // Clear removes the file, and says so once it has.
    again.clear();
    assert!(again.agg.as_ref().unwrap().is_empty());
    assert_eq!(cleared(&mut again), Ok(()));
    assert!(!log.exists());
}

/// Waits for the writer's answer to a clear.
fn cleared(h: &mut History) -> Result<(), String> {
    let until = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(r) = h.cleared() {
            return r;
        }
        assert!(std::time::Instant::now() < until, "the clear was never answered");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The `.tmp` files beside `log`.
fn temps(log: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(log.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    v.sort();
    v
}

/// Clear takes a compaction's leftover temporary files with it (they hold the same paths).
#[test]
fn clear_removes_compaction_leftovers_and_says_cleared_only_when_done() {
    let log = scratch("clear-tmp");
    std::fs::write(&log, format!("5\t1\t{}\n", p("/private").display())).unwrap();
    std::fs::write(log.with_extension("log.tmp"), "an old crash's").unwrap();
    std::fs::write(log.with_extension("log.4242-0.tmp"), "another process's").unwrap();
    let other = log.with_file_name("notes.tmp");
    std::fs::write(&other, "not ours").unwrap();
    let mut h = History::new(Some(log.clone()));
    h.clear();
    assert_eq!(cleared(&mut h), Ok(()));
    assert!(!log.exists());
    assert_eq!(temps(&log), ["notes.tmp"], "only the history's own");
}

#[cfg(unix)]
#[test]
fn a_clear_that_fails_says_so_and_keeps_the_history_readable() {
    use std::os::unix::fs::PermissionsExt;
    let log = scratch("clear-ro");
    std::fs::write(&log, format!("5\t1\t{}\n", p("/kept").display())).unwrap();
    // The lock file first, so only the removal fails.
    std::fs::write(log.with_extension("lock"), "").unwrap();
    let dir = log.parent().unwrap().to_path_buf();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let root = std::fs::write(dir.join("probe"), "").is_ok();
    let mut s = crate::app::tests::state_in(&dir);
    *s.shared.history.borrow_mut() = History::new(Some(log.clone()));
    let mut tx = Tx::default();
    s.run(Cmd::ClearJump, &mut tx);
    s.answer(true, &mut tx);
    assert_eq!(s.note.as_deref(), Some("Clearing Jump history…"), "not cleared until the writer says so");
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while s.shared.history.borrow().clearing.is_some() && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(2));
        s.poll(&mut tx);
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    if root {
        return;
    }
    assert!(s.note.as_deref().unwrap().starts_with("Couldn't clear the Jump history"), "{:?}", s.note);
    assert!(log.exists());
    assert!(s.shared.history.borrow().agg.is_none(), "read again next time, not shown as empty");
}

/// A write that fails (here the log's folder is a file) is said in the status bar, once.
#[test]
fn a_failed_write_says_so_once() {
    let log = scratch("write-fails");
    let blocker = log.with_file_name("not-a-folder");
    std::fs::write(&blocker, "").unwrap();
    let mut h = History::new(Some(blocker.join("history.log")));
    h.record(&p("/a"));
    h.record(&p("/b"));
    assert!(h.drain(Duration::from_secs(5)));
    let e = h.write_error().expect("told");
    assert!(e.starts_with("Couldn't save the Jump history"), "{e}");
    assert_eq!(h.write_error(), None, "once, not for every visit");
}

/// Visits still queued at exit are written, within the drain's bound.
#[test]
fn queued_visits_are_written_before_exit() {
    let log = scratch("drain");
    let mut h = History::new(Some(log.clone()));
    for i in 0..1000 {
        h.record(&p(&format!("/d/{i}")));
    }
    assert!(h.drain(Duration::from_secs(10)));
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1000, "none lost");
}

/// Another process holding the history's lock (compacting) holds up an append until it's
/// done, and each compaction writes through a temporary file of its own.
#[test]
fn appends_wait_for_another_process_holding_the_lock() {
    let log = scratch("lock");
    std::fs::write(&log, "").unwrap();
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(log.with_extension("lock")).unwrap();
    lock.lock().unwrap();
    let mut h = History::new(Some(log.clone()));
    h.record(&p("/waits"));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "", "written while another held the lock");
    drop(lock);
    assert!(h.drain(Duration::from_secs(5)));
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
    let names: std::collections::HashSet<PathBuf> = (0..3).map(|_| persist::temp_beside(&log)).collect();
    assert_eq!(names.len(), 3, "each rewrite has a name of its own");
    assert!(names.iter().all(|n| n.to_string_lossy().contains(&format!(".{}-", std::process::id()))));
    rewrite(&log, "x\n").unwrap();
    assert!(temps(&log).is_empty(), "no temporary file left behind");
}

/// A log bigger than any read limit is read whole, so compacting it keeps its newest visits.
#[test]
fn a_huge_log_is_read_to_its_end() {
    /// 70 MB of old visits, then one new one, made as they're read.
    struct Huge {
        left: usize,
        tail: Option<Vec<u8>>,
        line: Vec<u8>,
    }
    impl Read for Huge {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.left == 0 {
                let Some(t) = self.tail.take() else { return Ok(0) };
                buf[..t.len()].copy_from_slice(&t);
                return Ok(t.len());
            }
            let n = (buf.len() / self.line.len()).min(self.left).max(1);
            for k in 0..n {
                buf[k * self.line.len()..(k + 1) * self.line.len()].copy_from_slice(&self.line);
            }
            self.left -= n;
            Ok(n * self.line.len())
        }
    }
    let line = format!("100\t1\t{}\n", p("/old/folder/with/a/longish/name").display()).into_bytes();
    let left = 70_000_000 / line.len();
    let tail = Some(format!("{}\t1\t{}\n", now(), p("/newest").display()).into_bytes());
    let (agg, lines) = Frecency::from_reader(std::io::BufReader::new(Huge { left, tail, line })).unwrap();
    assert_eq!(lines, left + 1);
    assert!(agg.get(&p("/newest")).is_some(), "the end of the log was read");
}

#[test]
fn a_long_log_is_compacted_when_loaded() {
    let log = scratch("compact");
    let mut text = String::new();
    for i in 0..(COMPACT_AT + 10) {
        text.push_str(&format!("{}\t1\t{}\n", now() - i as u64, p(&format!("/d/{}", i % (KEEP + 100))).display()));
    }
    std::fs::write(&log, &text).unwrap();
    let mut h = History::new(Some(log.clone()));
    assert!(h.load(Duration::from_secs(10), None));
    // Over MAX_AGE in ranks, so aging dropped the folders seen once; the rest stay in the
    // session, and the file keeps the top KEEP of them.
    let n = h.agg.as_ref().unwrap().len();
    assert!(n > 0);
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), n.min(KEEP));
    assert!(temps(&log).is_empty());
}

/// Opening waits at most `OPEN_WAIT`; a slow load lands later through `poll`, with the
/// visits made meanwhile.
#[test]
fn a_slow_load_doesnt_hold_up_the_box() {
    let mut h = History::new(None);
    // Occupy the writer so the load waits behind it.
    let (block_tx, block_rx) = std::sync::mpsc::channel::<()>();
    let (tx, rx) = std::sync::mpsc::channel();
    let block_rx = std::sync::Mutex::new(block_rx);
    let _ = h.store().tx.send(Msg::Load(
        tx,
        Some(std::sync::Arc::new(move || {
            let _ = block_rx.lock().unwrap().recv_timeout(Duration::from_secs(5));
        })),
    ));
    let t = std::time::Instant::now();
    assert!(!h.load(OPEN_WAIT, None));
    assert!(t.elapsed() < Duration::from_millis(150), "waited {:?}", t.elapsed());
    assert!(h.is_loading());
    h.record(&p("/during"));
    block_tx.send(()).unwrap();
    drop(rx);
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while !h.poll() && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(h.agg.as_ref().unwrap().get(&p("/during")).is_some(), "a visit made while loading isn't lost");
}

// ---- through the app -----------------------------------------------------------------

fn dirs(name: &str, subs: &[&str]) -> PathBuf {
    let d = std::env::temp_dir().join(format!("arbor-jump-app-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    for s in subs {
        std::fs::create_dir_all(d.join(s)).unwrap();
    }
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Startup reads and writes nothing: no writer thread, no aggregate, no load. The first
/// navigation appends one line, on the writer.
#[test]
fn startup_does_no_history_io_and_a_visit_appends() {
    let d = dirs("startup", &["a"]);
    let mut s = crate::app::tests::state_in(&d);
    assert_eq!(s.shared.history.borrow().io(), 0);
    assert!(
        s.shared.history.borrow().store.is_none() && s.shared.history.borrow().agg.is_none() && !s.shared.history.borrow().is_loading()
    );
    let log = d.join("history.log");
    s.shared.history.borrow_mut().log = Some(log.clone());
    s.go(d.join("a"), Nav::New, None);
    crate::app::tests::settle_all(&mut s);
    settle_io(&s.shared.history.borrow(), 1);
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.ends_with(&format!("\t1\t{}\n", d.join("a").display())), "{text}");
    // A reload isn't a visit.
    s.reload();
    crate::app::tests::settle_all(&mut s);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(s.shared.history.borrow().io(), 1);
}

#[test]
fn jump_goes_to_the_best_match_and_drops_a_folder_thats_gone() {
    let d = dirs("go", &["projects/arbor", "projects/rune", "gone"]);
    let mut s = crate::app::tests::state_in(&d);
    let mut tx = Tx::default();
    for sub in ["projects/arbor", "projects/rune", "gone", "projects/arbor"] {
        s.go(d.join(sub), Nav::New, None);
        crate::app::tests::settle_all(&mut s);
    }
    s.go(d.clone(), Nav::New, None);
    crate::app::tests::settle_all(&mut s);
    s.run(Cmd::Jump, &mut tx);
    assert!(s.jump.as_ref().is_some_and(|j| !j.loading), "a small history loads within the wait");
    assert_eq!(s.jump.as_ref().unwrap().hits.len(), 4, "everything visited, by frecency");
    s.jump_edit().insert("proj arb");
    s.jump_changed(&mut tx);
    assert_eq!(*s.jump.as_ref().unwrap().hits[0].path, *d.join("projects/arbor"));
    s.jump_pick(0, &mut tx);
    assert!(s.jump.is_none());
    crate::app::tests::settle_all(&mut s);
    assert_eq!(s.cur().cwd, d.join("projects/arbor"));
    // A folder deleted since: going there says so and forgets it.
    std::fs::remove_dir(d.join("gone")).unwrap();
    s.run(Cmd::Jump, &mut tx);
    s.jump_edit().insert("gone");
    s.jump_changed(&mut tx);
    assert_eq!(*s.jump.as_ref().unwrap().hits[0].path, *d.join("gone"));
    s.jump_pick(0, &mut tx);
    crate::app::tests::settle_all(&mut s);
    assert_eq!(s.cur().cwd, d.join("projects/arbor"), "stays put");
    assert!(s.note.as_deref().is_some_and(|n| n.contains("no longer exists")), "{:?}", s.note);
    assert!(s.shared.history.borrow().agg.as_ref().unwrap().get(&d.join("gone")).is_none());
}

/// A path typed in Jump goes there, as go-to does.
#[test]
fn a_typed_path_goes_there() {
    let d = dirs("typed", &["x/y"]);
    let mut s = crate::app::tests::state_in(&d);
    let mut tx = Tx::default();
    s.run(Cmd::Jump, &mut tx);
    s.jump_edit().insert(&d.join("x").join("y").display().to_string());
    s.jump_changed(&mut tx);
    assert!(s.jump.as_ref().unwrap().path_mode());
    s.jump_field_key(&KeyEvent { key: Key::Enter, mods: rune::Mods::default() }, &mut tx);
    crate::app::tests::settle_all(&mut s);
    assert_eq!(s.cur().cwd, d.join("x").join("y"));
}

#[test]
fn jump_off_records_nothing_and_cant_open() {
    let d = dirs("off", &["a"]);
    let mut s = crate::app::tests::state_in(&d);
    s.shared.history.borrow_mut().enabled = false;
    s.go(d.join("a"), Nav::New, None);
    crate::app::tests::settle_all(&mut s);
    assert!(s.shared.history.borrow().store.is_none());
    assert!(s.unavailable(Cmd::Jump).is_some());
}

/// Ranking a 5k-folder history for one key: `cargo test --release -- --ignored --nocapture rank_speed`.
#[test]
#[ignore = "benchmark"]
fn rank_speed() {
    let now = now();
    let mut f = Frecency::default();
    let words = ["projects", "arbor", "src", "photos", "2024", "work", "client", "notes", "music", "build"];
    for i in 0..5_000u64 {
        let path =
            format!("/home/user/{}/{}-{i}/{}", words[(i % 10) as usize], words[(i / 10 % 10) as usize], words[(i / 100 % 10) as usize]);
        visit(&mut f, &path, now - i * 600, 1 + (i % 7) as usize);
    }
    for q in ["", "a", "proj arb", "photos 20"] {
        let t = std::time::Instant::now();
        let n = 20;
        for _ in 0..n {
            std::hint::black_box(f.rank(q, now, LIMIT));
        }
        println!("rank {q:?}: {:.2} ms", t.elapsed().as_secs_f64() * 1e3 / n as f64);
    }
}

/// Jump's handlers on a 5k history: `cargo test --release -- --ignored --nocapture open_speed`.
#[test]
#[ignore = "benchmark"]
fn open_speed() {
    let d = dirs("open-speed", &[]);
    let mut s = crate::app::tests::state_in(&d);
    let now = now();
    let mut log = String::new();
    let words = ["projects", "arbor", "src", "photos", "2024", "work", "client", "notes", "music", "build"];
    for i in 0..5_000usize {
        let w = |k: usize| words[(i / k) % 10];
        log.push_str(&format!("{}\t1\t{}\n", now - i as u64 * 517, d.join(w(1)).join(format!("{}-{i}", w(10))).join(w(100)).display()));
    }
    let t = std::time::Instant::now();
    let (agg, _) = Frecency::from_log(&log);
    println!("aggregate 5k lines: {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
    s.shared.history.borrow_mut().agg = Some(agg);
    let mut tx = Tx::default();
    for _ in 0..3 {
        let t = std::time::Instant::now();
        s.open_jump(&mut tx);
        println!("open (warm): {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
        for c in ["a", "r"] {
            let t = std::time::Instant::now();
            s.jump_key(&KeyEvent { key: Key::Text(c.into()), mods: rune::Mods::default() }, &mut tx);
            println!("type {c}: {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
        }
        s.close_jump(&mut tx);
    }
}

// ---- robustness of history.log -------------------------------------------------------------

/// Random path-ish text for the log: separators, tabs, CR, NUL and non-ASCII among names.
fn log_path(r: &mut crate::prop::Rng) -> String {
    let mut s = String::from(if cfg!(windows) { "C:\\" } else { "/" });
    for _ in 0..r.len(6) {
        s.push_str(&r.from(&["a", "Proj", "é", "😀", " ", "\t", "\r", "%", "\u{301}", "-", "."], 6));
        s.push(if cfg!(windows) { '\\' } else { '/' });
    }
    s.push_str(&r.string(8));
    s
}

/// The complete lines (ending in `\n`) of a log, as bytes without their line break.
fn whole_lines(b: &[u8]) -> Vec<&[u8]> {
    let mut v: Vec<&[u8]> = b.split_inclusive(|&c| c == b'\n').filter(|l| l.ends_with(b"\n")).collect();
    for l in &mut v {
        *l = &l[..l.len() - 1];
        // The reader drops every CR before the line break (Arbor never writes one in a path).
        while l.ends_with(b"\r") {
            *l = &l[..l.len() - 1];
        }
    }
    v
}

/// Any bytes (torn lines, NULs, CRLF, invalid UTF-8): loading never panics, and every folder
/// it finds is a full path written exactly, byte for byte, as the last field of a whole line,
/// never a guess (such as invalid bytes turned into U+FFFD).
#[test]
fn any_log_loads_only_paths_written_exactly() {
    crate::prop::check("history_any_bytes", 600, |r| {
        let mut log: Vec<u8> = Vec::new();
        for _ in 0..r.len(12) {
            match r.below(5) {
                0 => log.extend(r.bytes(40)),
                1 => {
                    let mut l =
                        format!("{}\t{}\t{}", r.below(1 << 20), r.pick(&["1", "-", "0.5", "nan", "inf", "-1", "1e308", ""]), log_path(r))
                            .into_bytes();
                    // A torn or damaged byte somewhere.
                    if r.one_in(2) && !l.is_empty() {
                        let i = r.below(l.len());
                        l[i] = r.byte();
                    }
                    log.extend(l);
                }
                _ => log.extend(format!("{}\t1\t{}", r.below(1000), log_path(r)).into_bytes()),
            }
            log.extend(r.pick(&[&b"\n"[..], b"\r\n", b"\n", b"", b"\0\n"]).iter());
        }
        let (f, lines) = Frecency::from_reader(&log[..]).unwrap();
        let whole = whole_lines(&log);
        assert_eq!(lines, whole.len());
        for d in &f.dirs {
            assert!(d.path.is_absolute() || d.path.has_root(), "{:?}", d.path);
            let p = d.path.to_str().expect("paths from the log are UTF-8");
            assert!(!p.contains('\0'));
            assert!(
                whole.iter().any(|l| l.splitn(3, |&c| c == b'\t').nth(2) == Some(p.as_bytes())),
                "{p:?} isn't written exactly in {:?}",
                String::from_utf8_lossy(&log)
            );
            assert!(d.rank.is_finite() && d.rank > 0.0, "{d:?}");
        }
    });
}

/// What the log writes reads back as the same folders; compacting keeps them.
#[test]
fn written_events_read_back_and_survive_compaction() {
    crate::prop::check("history_round_trip", 300, |r| {
        let mut want = Frecency::default();
        let mut log = String::new();
        let paths: Vec<PathBuf> = (0..1 + r.below(8)).map(|_| PathBuf::from(log_path(r))).collect();
        for _ in 0..r.len(40) {
            let path = r.pick(&paths).clone();
            let at = r.below(1 << 30) as u64;
            let e = if r.one_in(6) { Event::Drop { path } } else { Event::Visit { path, at, rank: 1.0 } };
            if let Some(line) = e.line(at) {
                log.push_str(&line);
                want.apply(&e);
            }
        }
        let (got, _) = Frecency::from_log(&log);
        let set = |f: &Frecency| {
            let mut v: Vec<(PathBuf, u64)> = f.dirs.iter().map(|d| (d.path.to_path_buf(), (d.rank * 1000.0).round() as u64)).collect();
            v.sort();
            v
        };
        assert_eq!(set(&got), set(&want), "{log:?}");
        let (again, _) = Frecency::from_log(&got.compacted(1 << 31));
        assert_eq!(set(&again), set(&want), "compacted");
    });
}

/// Found by `any_log_loads_only_paths_written_exactly`: a damaged byte in a line was read as
/// U+FFFD, so Jump offered a folder nobody visited ("/a\u{FFFD}b" for "/a\xFFb").
#[test]
fn a_line_that_isnt_utf8_is_skipped_not_guessed() {
    let root = if cfg!(windows) { "C:\\" } else { "/" };
    let log = [format!("1\t1\t{root}a").into_bytes(), b"\xff\n".to_vec(), format!("2\t1\t{root}ok\n").into_bytes()].concat();
    let (f, lines) = Frecency::from_reader(&log[..]).unwrap();
    assert_eq!(lines, 2);
    assert_eq!(f.dirs.iter().map(|d| d.path.to_path_buf()).collect::<Vec<_>>(), [PathBuf::from(format!("{root}ok"))]);
}

/// Found by `written_events_read_back_and_survive_compaction`: a path with a NUL was written,
/// and the reader (rightly) refused it, so the log and the session disagreed.
#[test]
fn a_path_with_a_nul_is_never_written() {
    assert_eq!(Event::Visit { path: p("/a\0b"), at: 1, rank: 1.0 }.line(1), None);
    assert!(Event::Visit { path: p("/ab"), at: 1, rank: 1.0 }.line(1).is_some());
}

/// Found reading `from_reader`: a huge line (a damaged log, or something else's file) was held
/// whole in memory however long it was. Now a line over `MAX_LINE` is read past, holding at
/// most that much, and the lines after it still count.
#[test]
fn a_huge_line_is_skipped_without_holding_it() {
    /// 16 MB of one line without a break, then a good line, handed out in 1 MB reads.
    struct Endless(usize);
    impl std::io::Read for Endless {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let n = out.len().min(self.0);
            out[..n].fill(b'a');
            self.0 -= n;
            Ok(n)
        }
    }
    let root = if cfg!(windows) { "C:\\" } else { "/" };
    let tail = format!("\n5\t1\t{root}after\n");
    let r = std::io::BufReader::with_capacity(1 << 20, std::io::Read::chain(Endless(16 << 20), std::io::Cursor::new(tail.into_bytes())));
    let (f, lines) = crate::prop::within(Duration::from_secs(10), "a 16 MB line", move || Frecency::from_reader(r).unwrap());
    assert_eq!(lines, 2);
    assert_eq!(f.dirs.iter().map(|d| d.path.to_path_buf()).collect::<Vec<_>>(), [PathBuf::from(format!("{root}after"))]);
    // The buffer never grows past the limit (plus one read).
    let mut r = std::io::BufReader::with_capacity(1 << 16, std::io::Read::chain(Endless(4 * MAX_LINE), "\nx\n".as_bytes()));
    let mut buf = Vec::new();
    assert_eq!(read_line(&mut r, &mut buf).unwrap(), Line::TooLong);
    assert!(buf.capacity() <= MAX_LINE + (1 << 16), "{}", buf.capacity());
    assert_eq!(read_line(&mut r, &mut buf).unwrap(), Line::Whole);
    assert_eq!(buf, b"x\n");
    assert_eq!(read_line(&mut r, &mut buf).unwrap(), Line::End);
    // A line of exactly the limit is still read.
    let line = format!("1\t1\t{root}{}\n", "b".repeat(MAX_LINE - 5 - root.len()));
    assert_eq!(line.len(), MAX_LINE);
    let (f, _) = Frecency::from_log(&line);
    assert_eq!(f.len(), 1);
}

/// The history's writer is every window's: once the first window closes, a clear finishing
/// wakes a window still open (review: it held the first window's waker forever, so the clear's
/// "cleared" waited for some other wake).
#[test]
fn the_history_wakes_an_open_window_after_the_first_closes() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let d = dirs("live-waker", &[]);
    let mut a = crate::app::tests::state_in(&d);
    crate::app::tests::settle_all(&mut a);
    let (aw, bw) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let n = aw.clone();
    a.setup(Arc::new(move || {
        n.fetch_add(1, Ordering::SeqCst);
    }));
    let mut b = a.new_window(d.clone());
    crate::app::tests::settle_all(&mut b);
    b.poll(&mut Tx::default());
    let n = bw.clone();
    b.setup(Arc::new(move || {
        n.fetch_add(1, Ordering::SeqCst);
    }));
    b.shared.history.borrow_mut().record(&d);
    assert!(b.shared.history.borrow().drain(Duration::from_secs(5)));
    a.leave();
    aw.store(0, Ordering::SeqCst);
    bw.store(0, Ordering::SeqCst);
    b.clear_jump_history();
    assert!(b.shared.history.borrow().drain(Duration::from_secs(5)));
    assert!(bw.load(Ordering::SeqCst) > 0, "the open window is woken");
    assert_eq!(aw.load(Ordering::SeqCst), 0, "the closed one isn't");
}
