//! Startup, listing and key-latency benchmark against the speed budgets in NORTHSTAR.md.
//! Run through `scripts/bench.sh`, which builds first:
//!
//! ```text
//! bench [RUNS] [ROUNDS] [--bin PATH] [--keep]
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

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ENTRIES: usize = 10_000;

fn main() {
    let mut nums = Vec::new();
    let mut bin = PathBuf::from(format!("target/release/arbor{}", std::env::consts::EXE_SUFFIX));
    let mut keep = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bin" => bin = args.next().map(PathBuf::from).expect("--bin PATH"),
            "--keep" => keep = true,
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
        match run_once(&bin, &big, rounds) {
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
    if !keep {
        let _ = std::fs::remove_dir_all(&root);
    }

    println!("{} {} · {runs} runs × {rounds} rounds · {ENTRIES} entries", std::env::consts::OS, std::env::consts::ARCH);
    println!("{:<34} {:>5} {:>9} {:>9} {:>8}  result (submission proxy)", "measure", "n", "median", "p95", "budget");
    let rows: [(&str, &mut Vec<f64>, Option<f64>); 4] = [
        ("cold start (spawn → first frame)*", &mut cold, Some(100.0)),
        ("  of which main → first frame", &mut main, None),
        ("listing visible (10k, nav → frame)", &mut nav, Some(50.0)),
        ("key → frame (synthetic ↓)", &mut key, Some(8.0)),
    ];
    for (name, v, budget) in rows {
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

struct Run {
    cold: f64,
    main: f64,
    nav: Vec<f64>,
    key: Vec<f64>,
    /// When the first listing showed, if it missed the first frame (from `main`).
    first_listing: Option<f64>,
}

fn run_once(bin: &Path, dir: &Path, rounds: u32) -> Result<Run, String> {
    let spawned = SystemTime::now();
    let mut child = Command::new(bin)
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
    let mut run = Run { cold: f64::NAN, main: f64::NAN, nav: Vec::new(), key: Vec::new(), first_listing: None };
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
            Some("key") => run.key.extend(field("ms")),
            _ => {}
        }
    }
    if run.cold.is_nan() || run.main.is_nan() {
        return Err(format!("no first frame logged:\n{}", lines.join("\n")));
    }
    let want = rounds as usize;
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

/// The `p`th percentile of sorted `v` (nearest rank).
fn pct(v: &[f64], p: f64) -> f64 {
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    v[rank.clamp(1, v.len()) - 1]
}
