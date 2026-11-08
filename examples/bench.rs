//! Startup, listing and key-latency benchmark against the speed budgets in NORTHSTAR.md.
//! Run through `scripts/bench.sh`, which builds first:
//!
//! ```text
//! bench [RUNS] [ROUNDS] [--bin PATH] [--keep] [--tabs N] [--preview N] [--jump N] [--search N] [--rename N]
//! ```
//!
//! Makes a folder of 10,000 entries in the temp dir, then cold-starts Arbor on it RUNS times
//! (default 10) with `ARBOR_BENCH=ROUNDS` (default 10): each run logs its first frame, then
//! ROUNDS times leaves the folder, comes back (a fresh 10k listing) and presses ↓, then quits.
//! Prints the median and p95 of each measure against its budget. The first run is reported on its
//! own and left out of the cold-start numbers: on macOS the first exec of a newly built binary
//! spends hundreds of milliseconds before `main` (the system checks its signature), which a user
//! pays once per install or update.
//!
//! "Cold start" is from just before this process spawns Arbor to Arbor's first frame being
//! submitted, so it includes exec, dynamic linking, window and GPU setup and reading the
//! folder. The folder's metadata is in the OS cache after the first run (a true cold cache
//! needs root to drop it). The window waits only a bounded time for the folder, so each run
//! also says whether the listing made the first frame, and when it showed if it didn't.
//!
//! All times end when the frame is submitted to the GPU: a proxy for pixels on screen, which
//! come at least one refresh later.
//!
//! A run counts only if Arbor exits successfully and logs its first frame and exactly ROUNDS
//! listings and ROUNDS key presses. Otherwise the run is reported INVALID, and the bench exits
//! non-zero after printing what it has.
//!
//! `--tabs N` adds N more cold starts with `ARBOR_BENCH_TABS=1`: each opens a second tab on the
//! parent folder, then ROUNDS times shows the first tab again (a fresh read of the 10k folder,
//! timed from the key to the frame showing it) and goes back to the second. Such a run counts
//! only with exactly ROUNDS tab switches to the 10k folder logged.
//!
//! `--preview N` adds N more cold starts with `ARBOR_BENCH_PREVIEW=1` in a folder holding a
//! 300 KB text file (the preview shows its first 256 KB) and a 12 MP JPEG (4000 × 3000). Each
//! round previews the text and then the photo, each timed from Space to the frame showing it,
//! and moves the selection while the preview is open, once while the photo is still decoding
//! (see `src/timing.rs`). Such a run counts only with exactly 8 keys, one text preview and
//! one photo preview logged per round.
//!
//! `--jump N` adds N more cold starts with `ARBOR_BENCH_JUMP=1` and a Jump history of
//! [`JUMP_FOLDERS`] folders: each round opens Jump to Folder (timed from the key to the frame
//! showing the ranked folders; the first open of a run reads and aggregates the log, later ones
//! rank what's in memory), types two letters (each key timed, ranking all of them again) and
//! closes it. Such a run counts only with exactly ROUNDS opens (one cold) and 4 keys a round.
//!
//! `--search N` adds N more cold starts with `ARBOR_BENCH_SEARCH=1` in a tree of
//! [`SEARCH_FILES`] files in [`SEARCH_DIRS`] folders: each round opens Search in This Folder,
//! types `1` (about half the names match, so results stream for many frames), presses ↓ once a
//! frame while the walk streams, and leaves when it's done. It reports the key to the first
//! results and to the end of the walk, the query key itself (which starts the walk), and every
//! ↓ pressed while results streamed in. Such a run counts only with exactly ROUNDS first
//! results and ROUNDS finished walks logged.
//!
//! `--rename N` adds N more cold starts with `ARBOR_BENCH_RENAME=1` in the 10k folder: each
//! round selects all 10,000 entries, opens Batch Rename, types `e` (plain text: every name
//! loses its e's), turns on regex and types `$` (`e$`), each timed from the key to the frame
//! showing the whole preview for it (worked out on a worker at this size), and closes it. Such
//! a run counts only with exactly ROUNDS plain and 2 × ROUNDS regex previews of 10,000 items.
//!
//! Every run gets its own settings folder (`RUNE_DIRS_ROOT`), so the user's settings don't
//! change the numbers and the navigations don't land in their Jump history.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ENTRIES: usize = 10_000;
/// Folders in the `--jump` history.
const JUMP_FOLDERS: usize = 5_000;
/// The `--search` tree: files, in folders two levels deep.
const SEARCH_FILES: usize = 120_000;
const SEARCH_DIRS: usize = 2_000;

fn main() {
    let mut nums = Vec::new();
    let mut bin = PathBuf::from(format!("target/release/arbor{}", std::env::consts::EXE_SUFFIX));
    let mut keep = false;
    let mut tab_runs = 0;
    let mut preview_runs = 0;
    let mut jump_runs = 0;
    let mut search_runs = 0;
    let mut rename_runs = 0;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bin" => bin = args.next().map(PathBuf::from).expect("--bin PATH"),
            "--keep" => keep = true,
            "--tabs" => tab_runs = args.next().and_then(|n| n.parse::<u32>().ok()).expect("--tabs N"),
            "--preview" => preview_runs = args.next().and_then(|n| n.parse::<u32>().ok()).expect("--preview N"),
            "--jump" => jump_runs = args.next().and_then(|n| n.parse::<u32>().ok()).expect("--jump N"),
            "--search" => search_runs = args.next().and_then(|n| n.parse::<u32>().ok()).expect("--search N"),
            "--rename" => rename_runs = args.next().and_then(|n| n.parse::<u32>().ok()).expect("--rename N"),
            n => nums.push(n.parse::<u32>().unwrap_or_else(|_| panic!("not a number: {n}"))),
        }
    }
    let runs = nums.first().copied().unwrap_or(10);
    let rounds = nums.get(1).copied().unwrap_or(10);
    assert!(bin.exists(), "{} not found: build with `cargo build --release` first", bin.display());

    let root = std::env::temp_dir().join(format!("arbor-bench-{}", std::process::id()));
    let big = root.join("big");
    make_folder(&big);

    let (mut cold, mut main, mut nav, mut key) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut each, mut late, mut invalid) = (Vec::new(), Vec::new(), Vec::new());
    let mut first = None;
    for run in 0..runs {
        match run_once(&bin, &big, rounds, Mode::Nav) {
            Ok(r) => {
                each.push(format!("{:.0}/{:.0}", r.cold, r.main));
                if let Some(ms) = r.first_listing {
                    late.push(format!("run {run}: {ms:.0} ms from main"));
                }
                if run == 0 && runs > 1 {
                    first = Some((r.cold, r.main));
                } else {
                    cold.push(r.cold);
                    main.push(r.main);
                }
                nav.extend(r.nav);
                key.extend(r.key);
            }
            Err(e) => {
                eprintln!("run {run}: INVALID: {e}");
                invalid.push(run);
            }
        }
    }
    let (mut tab, mut same) = (Vec::new(), 0);
    for run in 0..tab_runs {
        match run_once(&bin, &big, rounds, Mode::Tabs) {
            Ok(r) => {
                same += r.same_frame;
                tab.extend(r.tab);
            }
            Err(e) => {
                eprintln!("tab run {run}: INVALID: {e}");
                invalid.push(runs + run);
            }
        }
    }
    let (mut text, mut photo, mut open_key, mut decoding_key) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    if preview_runs > 0 {
        let dir = root.join("preview");
        make_preview_folder(&dir);
        for run in 0..preview_runs {
            match run_once(&bin, &dir, rounds, Mode::Preview) {
                Ok(r) => {
                    text.extend(r.preview_text);
                    photo.extend(r.preview_photo);
                    open_key.extend(r.key_open);
                    decoding_key.extend(r.key_decoding);
                }
                Err(e) => {
                    eprintln!("preview run {run}: INVALID: {e}");
                    invalid.push(runs + tab_runs + run);
                }
            }
        }
    }
    let (mut jump_cold, mut jump_warm, mut jump_key, mut jump_same) = (Vec::new(), Vec::new(), Vec::new(), 0);
    if jump_runs > 0 {
        make_jump_history(&root.join("dirs-jump"), &root);
        for run in 0..jump_runs {
            match run_once(&bin, &big, rounds, Mode::Jump) {
                Ok(r) => {
                    jump_cold.extend(r.jump_cold);
                    jump_warm.extend(r.jump_warm);
                    jump_key.extend(r.jump_key);
                    jump_same += r.same_frame;
                }
                Err(e) => {
                    eprintln!("jump run {run}: INVALID: {e}");
                    invalid.push(runs + tab_runs + preview_runs + run);
                }
            }
        }
    }
    let (mut search_first, mut search_total, mut search_query, mut search_key) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut search_found = Vec::new();
    if search_runs > 0 {
        let dir = root.join("tree");
        make_search_tree(&dir);
        for run in 0..search_runs {
            match run_once(&bin, &dir, rounds, Mode::Search) {
                Ok(r) => {
                    search_first.extend(r.search_first);
                    search_total.extend(r.search_total);
                    search_query.extend(r.search_query);
                    search_key.extend(r.search_key);
                    search_found.extend(r.search_found);
                }
                Err(e) => {
                    eprintln!("search run {run}: INVALID: {e}");
                    invalid.push(runs + tab_runs + preview_runs + jump_runs + run);
                }
            }
        }
    }
    let (mut rename_plain, mut rename_regex, mut rename_key) = (Vec::new(), Vec::new(), Vec::new());
    for run in 0..rename_runs {
        match run_once(&bin, &big, rounds, Mode::Rename) {
            Ok(r) => {
                rename_plain.extend(r.rename_plain);
                rename_regex.extend(r.rename_regex);
                rename_key.extend(r.rename_key);
            }
            Err(e) => {
                eprintln!("rename run {run}: INVALID: {e}");
                invalid.push(runs + tab_runs + preview_runs + jump_runs + search_runs + run);
            }
        }
    }
    if !keep {
        let _ = std::fs::remove_dir_all(&root);
    }

    println!("{} {} · {runs} runs × {rounds} rounds · {ENTRIES} entries", std::env::consts::OS, std::env::consts::ARCH);
    println!("{:<34} {:>5} {:>9} {:>9} {:>8}  result (submission proxy)", "measure", "n", "median", "p95", "budget");
    let tabs = tab_runs > 0;
    let previews = preview_runs > 0;
    let jumps = jump_runs > 0;
    let searches = search_runs > 0;
    let renames = rename_runs > 0;
    let rows: [(&str, &mut Vec<f64>, Option<f64>); 19] = [
        ("cold start (spawn → first frame)*", &mut cold, Some(100.0)),
        ("  of which main → first frame", &mut main, None),
        ("listing visible (10k, nav → frame)", &mut nav, Some(50.0)),
        ("key → frame (synthetic ↓)", &mut key, Some(8.0)),
        ("tab switch to 10k (key → frame)", &mut tab, Some(50.0)),
        ("preview 256 KB text (Space → frame)", &mut text, None),
        ("preview 12 MP JPEG (Space → frame)", &mut photo, None),
        ("key → frame, preview open", &mut open_key, Some(8.0)),
        ("  … while the JPEG decodes", &mut decoding_key, Some(8.0)),
        ("jump open, 5k history, cold", &mut jump_cold, None),
        ("jump open, 5k history, warm", &mut jump_warm, Some(8.0)),
        ("key → frame, typing in Jump (5k)", &mut jump_key, Some(8.0)),
        ("search 120k: key → first results", &mut search_first, None),
        ("search 120k: key → walk done", &mut search_total, None),
        ("key → frame, query (starts walk)", &mut search_query, Some(8.0)),
        ("key → frame while results stream", &mut search_key, Some(8.0)),
        ("rename 10k: key → preview (plain)", &mut rename_plain, None),
        ("rename 10k: key → preview (regex)", &mut rename_regex, None),
        ("key → frame, typing in rename (10k)", &mut rename_key, Some(8.0)),
    ];
    for (name, v, budget) in rows {
        let off = (name.starts_with("tab") && !tabs)
            || ((name.contains("preview") || name.contains("JPEG")) && !previews)
            || (name.contains("ump") && !jumps)
            || ((name.contains("search") || name.contains("stream") || name.contains("query")) && !searches)
            || (name.contains("rename") && !renames);
        if v.is_empty() && off {
            continue;
        }
        if v.is_empty() {
            println!("{name:<34} {:>5}", 0);
            continue;
        }
        v.sort_by(f64::total_cmp);
        let (med, p95) = (pct(v, 50.0), pct(v, 95.0));
        let (b, verdict) = match budget {
            Some(b) => (
                format!("{b:.0} ms"),
                if p95 < b {
                    "under"
                } else if med < b {
                    "p95 over"
                } else {
                    "OVER"
                },
            ),
            None => (String::new(), ""),
        };
        println!("{name:<34} {:>5} {med:>6.1} ms {p95:>6.1} ms {b:>8}  {verdict}", v.len());
    }
    if tabs {
        println!("tab switches whose listing was in the key's own frame: {same} of {}", tab.len());
    }
    if jumps {
        println!("Jump opens whose rows were in the key's own frame: {jump_same} of {}", jump_cold.len() + jump_warm.len());
    }
    if searches {
        search_found.sort_unstable();
        println!(
            "search walks: {} found of {} items each (median); {} ↓ keys while streaming",
            search_found.get(search_found.len() / 2).map_or(0, |f| f.0),
            search_found.get(search_found.len() / 2).map_or(0, |f| f.1),
            search_key.len()
        );
    }
    if let Some((c, m)) = first {
        println!("* first run left out: {c:.0} ms cold, {m:.0} ms of it from main");
    }
    println!("each run, cold/main ms: {}", each.join(" "));
    let valid = each.len();
    println!(
        "listing in the first frame: {} of {valid} runs{}",
        valid - late.len(),
        if late.is_empty() { String::new() } else { format!(" (late: {})", late.join(", ")) }
    );
    println!("Times end when the frame is submitted to the GPU (a proxy): pixels reach the screen at least one refresh later.");
    if !invalid.is_empty() {
        println!("INVALID: {} of {runs} runs ({invalid:?}); see above. These numbers don't count.", invalid.len());
        std::process::exit(1);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Nav,
    Tabs,
    Preview,
    Jump,
    Search,
    Rename,
}

struct Run {
    cold: f64,
    main: f64,
    nav: Vec<f64>,
    key: Vec<f64>,
    /// When the first listing showed, if it missed the first frame (from `main`).
    first_listing: Option<f64>,
    /// Tab switches to the 10k folder, and how many landed in the key's own frame.
    tab: Vec<f64>,
    same_frame: usize,
    /// Space → frame showing the text or the photo; keys while the preview is open, and those
    /// while the photo decodes.
    preview_text: Vec<f64>,
    preview_photo: Vec<f64>,
    key_open: Vec<f64>,
    key_decoding: Vec<f64>,
    /// Jump opened, reading the log or ranking what's in memory; keys typed in it.
    jump_cold: Vec<f64>,
    jump_warm: Vec<f64>,
    jump_key: Vec<f64>,
    /// Search: key → first results, key → walk done, the query key, ↓ while streaming, and
    /// (found, visited) for each walk.
    search_first: Vec<f64>,
    search_total: Vec<f64>,
    search_query: Vec<f64>,
    search_key: Vec<f64>,
    search_found: Vec<(u64, u64)>,
    /// Batch Rename over 10k items: key → preview frame, plain and regex; keys typed in it.
    rename_plain: Vec<f64>,
    rename_regex: Vec<f64>,
    rename_key: Vec<f64>,
}

fn run_once(bin: &Path, dir: &Path, rounds: u32, mode: Mode) -> Result<Run, String> {
    let spawned = SystemTime::now();
    let mut cmd = Command::new(bin);
    // Its own settings folder: the Jump history for `--jump`, an empty one otherwise.
    let dirs = dir.parent().unwrap_or(dir).join(if mode == Mode::Jump { "dirs-jump" } else { "dirs" });
    cmd.env("RUNE_DIRS_ROOT", dirs);
    for (var, on) in [
        ("ARBOR_BENCH_TABS", mode == Mode::Tabs),
        ("ARBOR_BENCH_PREVIEW", mode == Mode::Preview),
        ("ARBOR_BENCH_JUMP", mode == Mode::Jump),
        ("ARBOR_BENCH_SEARCH", mode == Mode::Search),
        ("ARBOR_BENCH_RENAME", mode == Mode::Rename),
    ] {
        if on {
            cmd.env(var, "1");
        } else {
            cmd.env_remove(var);
        }
    }
    let mut child = cmd
        .arg(dir)
        .env("ARBOR_BENCH", rounds.to_string())
        .env_remove("ARBOR_TIMING")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let err = child.stderr.take().expect("piped");
    let reader = std::thread::spawn(move || BufReader::new(err).lines().map_while(Result::ok).collect::<Vec<_>>());
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                return Err("timed out after 60 s".into());
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let lines = reader.join().map_err(|_| "reader panicked")?;
    if !status.success() {
        return Err(format!("arbor exited with {status}:\n{}", lines.join("\n")));
    }
    let spawn_us = spawned.duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_micros() as f64;
    let mut run = Run {
        cold: f64::NAN,
        main: f64::NAN,
        nav: Vec::new(),
        key: Vec::new(),
        first_listing: None,
        tab: Vec::new(),
        same_frame: 0,
        preview_text: Vec::new(),
        preview_photo: Vec::new(),
        key_open: Vec::new(),
        key_decoding: Vec::new(),
        jump_cold: Vec::new(),
        jump_warm: Vec::new(),
        jump_key: Vec::new(),
        search_first: Vec::new(),
        search_total: Vec::new(),
        search_query: Vec::new(),
        search_key: Vec::new(),
        search_found: Vec::new(),
        rename_plain: Vec::new(),
        rename_regex: Vec::new(),
        rename_key: Vec::new(),
    };
    let mut keys = 0;
    let mut streaming = false;
    for line in &lines {
        let Some(rest) = line.strip_prefix("arbor-timing ") else { continue };
        let field = |k: &str| rest.split_whitespace().find_map(|f| f.strip_prefix(k)?.strip_prefix('=')?.parse::<f64>().ok());
        match rest.split_whitespace().next() {
            Some("first-frame") => {
                run.cold = (field("epoch_us").unwrap_or(f64::NAN) - spawn_us) / 1000.0;
                run.main = field("main_ms").unwrap_or(f64::NAN);
            }
            Some("first-listing") => run.first_listing = field("main_ms"),
            Some("nav") if field("entries").is_some_and(|n| n as usize >= ENTRIES) => run.nav.extend(field("ms")),
            Some("key") if mode == Mode::Preview => {
                // Each round's keys: Space, Space, ↓, Space, ↑, ↓, ↑, Space. The ↑ ↓ ↑ move
                // the selection with the preview open; the last ↑ comes while the photo decodes.
                match keys % 8 {
                    4 | 5 => run.key_open.extend(field("ms")),
                    6 => {
                        run.key_open.extend(field("ms"));
                        run.key_decoding.extend(field("ms"));
                    }
                    _ => {}
                }
                keys += 1;
            }
            Some("key") if mode == Mode::Jump => {
                // Each round's keys: open, a, r, Esc.
                if matches!(keys % 4, 1 | 2) {
                    run.jump_key.extend(field("ms"));
                }
                keys += 1;
            }
            Some("jump") if field("folders").is_some_and(|n| n as usize >= JUMP_FOLDERS) => {
                if field("cold") == Some(1.0) {
                    run.jump_cold.extend(field("ms"));
                } else {
                    run.jump_warm.extend(field("ms"));
                }
                run.same_frame += usize::from(field("same_frame") == Some(1.0));
            }
            Some("key") if mode == Mode::Search => {
                // The first key of a walk is the query; later ones while it streams are ↓.
                let walk = field("search") == Some(1.0);
                if walk && streaming {
                    run.search_key.extend(field("ms"));
                } else if walk {
                    run.search_query.extend(field("ms"));
                }
                streaming = walk;
            }
            Some("key") if mode == Mode::Rename => {
                // Each round's keys: select all, open, e, regex on, $, Esc.
                if matches!(keys % 6, 2..=4) {
                    run.rename_key.extend(field("ms"));
                }
                keys += 1;
            }
            Some("rename") if field("items").is_some_and(|n| n as usize >= ENTRIES) => {
                if field("regex") == Some(1.0) {
                    run.rename_regex.extend(field("ms"));
                } else {
                    run.rename_plain.extend(field("ms"));
                }
            }
            Some("search-first") => run.search_first.extend(field("ms")),
            Some("search-done") => {
                run.search_total.extend(field("ms"));
                run.search_found.push((field("found").unwrap_or(0.0) as u64, field("visited").unwrap_or(0.0) as u64));
            }
            Some("key") => run.key.extend(field("ms")),
            Some("preview") if rest.contains("from=open") && rest.contains("kind=text") => run.preview_text.extend(field("ms")),
            Some("preview") if rest.contains("from=open") && rest.contains("kind=image") => run.preview_photo.extend(field("ms")),
            Some("tab") if field("entries").is_some_and(|n| n as usize >= ENTRIES) => {
                run.tab.extend(field("ms"));
                run.same_frame += usize::from(field("same_frame") == Some(1.0));
            }
            _ => {}
        }
    }
    if run.cold.is_nan() || run.main.is_nan() {
        return Err(format!("no first frame logged:\n{}", lines.join("\n")));
    }
    let want = rounds as usize;
    if mode == Mode::Rename {
        if keys != want * 6 || run.rename_plain.len() != want || run.rename_regex.len() != want * 2 {
            return Err(format!(
                "expected {} keys, {want} plain and {} regex previews of {ENTRIES}, got {keys}, {} and {}:\n{}",
                want * 6,
                want * 2,
                run.rename_plain.len(),
                run.rename_regex.len(),
                lines.join("\n")
            ));
        }
        return Ok(run);
    }
    if mode == Mode::Search {
        if run.search_first.len() != want || run.search_total.len() != want {
            return Err(format!(
                "expected {want} first results and finished walks, got {} and {}:\n{}",
                run.search_first.len(),
                run.search_total.len(),
                lines.join("\n")
            ));
        }
        return Ok(run);
    }
    if mode == Mode::Jump {
        if keys != want * 4 || run.jump_cold.len() != 1 || run.jump_cold.len() + run.jump_warm.len() != want {
            return Err(format!(
                "expected {} keys and {want} Jump opens of {JUMP_FOLDERS} folders (one cold), got {keys}, {} cold and {} warm:\n{}",
                want * 4,
                run.jump_cold.len(),
                run.jump_warm.len(),
                lines.join("\n")
            ));
        }
        return Ok(run);
    }
    if mode == Mode::Preview {
        if keys != want * 8 || run.preview_text.len() != want || run.preview_photo.len() != want {
            return Err(format!(
                "expected {} keys and {want} text and photo previews, got {keys}, {} and {}:\n{}",
                want * 8,
                run.preview_text.len(),
                run.preview_photo.len(),
                lines.join("\n")
            ));
        }
        return Ok(run);
    }
    if mode == Mode::Tabs {
        if run.tab.len() != want {
            return Err(format!("expected {want} tab switches to {ENTRIES} entries, got {}:\n{}", run.tab.len(), lines.join("\n")));
        }
        return Ok(run);
    }
    if run.nav.len() != want || run.key.len() != want {
        return Err(format!(
            "expected {want} listings of {ENTRIES} and {want} key presses, got {} and {}:\n{}",
            run.nav.len(),
            run.key.len(),
            lines.join("\n")
        ));
    }
    Ok(run)
}

/// `ENTRIES` entries: a few hundred folders and files with mixed names and extensions.
fn make_folder(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).expect("temp dir");
    let exts = ["jpg", "txt", "pdf", "rs", "mp3", "png", "md", "zip"];
    for i in 0..ENTRIES {
        let p = match i % 40 {
            0 => dir.join(format!("Folder {i}")),
            n if n % 3 == 0 => dir.join(format!("IMG_{i:05}.{}", exts[i % exts.len()])),
            n if n % 3 == 1 => dir.join(format!("notes {i} draft.{}", exts[(i / 3) % exts.len()])),
            _ => dir.join(format!("report-{}-v{}.{}", i / 7, i % 7, exts[(i / 5) % exts.len()])),
        };
        let made = if i % 40 == 0 { std::fs::create_dir(&p) } else { std::fs::write(&p, b"x") };
        made.unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    }
}

/// [`SEARCH_FILES`] small files in [`SEARCH_DIRS`] folders, two levels deep (`d17/e5/`), named
/// like a project's (`report-1234.txt`, `IMG_1234.jpg`…). Made once per bench (a few seconds).
fn make_search_tree(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    let per = SEARCH_FILES / SEARCH_DIRS;
    let names = ["report", "IMG", "notes", "draft", "main", "photo"];
    let exts = ["txt", "jpg", "md", "rs", "png", "pdf"];
    for d in 0..SEARCH_DIRS {
        let sub = dir.join(format!("d{}", d / 50)).join(format!("e{}", d % 50));
        std::fs::create_dir_all(&sub).expect("search tree");
        for f in 0..per {
            let i = d * per + f;
            std::fs::write(sub.join(format!("{}-{i}.{}", names[i % names.len()], exts[(i / 7) % exts.len()])), b"x").expect("file");
        }
    }
}

/// `a.txt`, 300 KB of text, and `b.jpg`, a 4000 × 3000 photo-like JPEG (gradients, shapes and
/// noise, so it compresses like a photo: a few MB at quality 90).
fn make_preview_folder(dir: &Path) {
    use image::ImageEncoder;
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).expect("temp dir");
    let mut text = String::new();
    let mut i = 0;
    while text.len() < 300_000 {
        text.push_str(&format!("{i:06}\tfn line_{i}() -> usize {{ {} }} // some code-like text\n", i * 7 % 1000));
        i += 1;
    }
    std::fs::write(dir.join("a.txt"), text).expect("a.txt");
    let (w, h) = (4000u32, 3000u32);
    let mut seed = 0x2545_f491_u32;
    let mut px = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let n = (seed & 31) as i32 - 16;
            let disc = (((x / 250) + (y / 250)) % 3) as i32 * 40;
            let c = |v: i32| (v + n).clamp(0, 255) as u8;
            px.extend([c((x * 255 / w) as i32 + disc / 2), c((y * 255 / h) as i32), c(160 - disc)]);
        }
    }
    let mut jpg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, 90)
        .write_image(&px, w, h, image::ExtendedColorType::Rgb8)
        .expect("encode the photo");
    std::fs::write(dir.join("b.jpg"), jpg).expect("b.jpg");
}

/// A Jump history of [`JUMP_FOLDERS`] folders (one line each, two visits to every fifth, spread over
/// a month; ranks under `MAX_AGE`, so none age out), at `root`'s settings folder. The folders
/// needn't exist: Jump never looks.
fn make_jump_history(root: &Path, under: &Path) {
    let dir = root.join("config").join(if cfg!(any(target_os = "macos", windows)) { "Arbor" } else { "arbor" });
    std::fs::create_dir_all(&dir).expect("settings folder");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let words = ["projects", "arbor", "src", "photos", "2024", "work", "client", "notes", "music", "build"];
    let mut log = String::new();
    for i in 0..JUMP_FOLDERS {
        let w = |k: usize| words[(i / k) % words.len()];
        let p = under.join("home").join(w(1)).join(format!("{}-{i}", w(10))).join(w(100));
        log.push_str(&format!("{}\t{}\t{}\n", now - (i as u64 * 517) % (30 * 86_400), 1 + usize::from(i % 5 == 0), p.display()));
    }
    std::fs::write(dir.join("history.log"), log).expect("history.log");
}

/// The `p`th percentile of sorted `v` (nearest rank).
fn pct(v: &[f64], p: f64) -> f64 {
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    v[rank.clamp(1, v.len()) - 1]
}
