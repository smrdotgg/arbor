//! Opt-in measurement of the speed budgets in NORTHSTAR.md.
//!
//! `ARBOR_TIMING=1` logs to stderr, one line per event:
//!
//! ```text
//! arbor-timing first-frame main_ms=41.2 init_at_ms=3.1 init_ms=12.0 epoch_us=1762591234567890 entries=10000
//! arbor-timing key ms=1.9 update_ms=1.1 encode_ms=0.6 refresh_hz=120
//! arbor-timing nav ms=9.4 update_ms=3.2 encode_ms=0.7 entries=10000
//! ```
//!
//! - `first-frame`: from the top of `main` to the first frame submitted. `epoch_us` is that
//!   moment on the wall clock, so a launcher can measure from before `exec` (`scripts/bench.sh`).
//!   `init_ms` is the part spent in `State::new`, which reads the first folder; `init_at_ms` is
//!   when that started (Rune sets up its event loop first, then creates the window and GPU).
//! - `key`: from the key's arrival (the OS event's own timestamp on macOS) to the frame it
//!   changed being submitted.
//! - `nav`: from the request (the key or click that asked) to the frame showing the new listing.
//!
//! "Submitted" is when Rune handed the frame to the GPU (`FrameEncoded::at`). The pixels reach
//! the screen at the next refresh after that; Rune doesn't report when (see RUNE-GAPS.md).
//!
//! `ARBOR_BENCH=N` (implies timing) drives the app itself: after the first frame it does N
//! rounds of "go to the parent, come back, press ↓", each step started by the previous step's
//! frame (no timers), then quits. The ↓ is synthetic, so its `key` time starts when the handler
//! is called rather than at an OS event. The window opens without taking focus and ignores real
//! keys, so typing in another app while it runs can't reach it (and its ⌘⌫ can't trash anything).
//!
//! Off, nothing here runs: `main` reads the two variables once and, unless one is set, hands
//! Rune the plain callbacks and registers no frame hook. No flag is checked per frame or per key.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rune::{FrameEncoded, FrameOutcome, Key, KeyEvent, Mods, Tx};

use crate::app::{Nav, State};

/// When `main` started, on both clocks.
static MAIN: OnceLock<(Instant, SystemTime)> = OnceLock::new();
/// Wakes the window between bench steps.
static WAKER: OnceLock<rune::Waker> = OnceLock::new();

thread_local! {
    /// A handled key waiting for its frame: when it arrived.
    static KEY: Cell<Option<Instant>> = const { Cell::new(None) };
    /// The navigation in flight: when it was asked for.
    static NAV: Cell<Option<Instant>> = const { Cell::new(None) };
    /// A navigation whose listing lands in the frame being built: when it was asked for, and
    /// how many entries it has.
    static LANDED: Cell<Option<(Instant, usize)>> = const { Cell::new(None) };
    static FIRST: Cell<bool> = const { Cell::new(true) };
    /// Entries in the first listing.
    static FIRST_ENTRIES: Cell<usize> = const { Cell::new(0) };
    /// How long `State::new` took (it reads the first folder before the window opens).
    static INIT_MS: Cell<f64> = const { Cell::new(0.0) };
    /// When `State::new` started, from the top of `main` (Rune's event loop setup before it).
    static INIT_AT_MS: Cell<f64> = const { Cell::new(0.0) };
    /// Bench steps left, and the step counter.
    static BENCH: Cell<(u32, u32)> = const { Cell::new((0, 0)) };
}

/// Whether `ARBOR_BENCH` is driving the app (after `init`).
pub fn benching() -> bool {
    BENCH.get() != (0, 0)
}

/// Reads `ARBOR_TIMING` and `ARBOR_BENCH` once. True when timing is on.
pub fn init() -> bool {
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty() && v != "0");
    let rounds = std::env::var("ARBOR_BENCH").ok().and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(0);
    if !set("ARBOR_TIMING") && rounds == 0 {
        return false;
    }
    MAIN.get_or_init(|| (Instant::now(), SystemTime::now()));
    BENCH.set((rounds * 3, 0));
    true
}

pub fn init_state() -> State {
    let t = Instant::now();
    INIT_AT_MS.set(MAIN.get().map_or(0.0, |(m, _)| t.duration_since(*m).as_secs_f64() * 1000.0));
    let s = State::new();
    INIT_MS.set(t.elapsed().as_secs_f64() * 1000.0);
    s
}

pub fn setup(s: &mut State, waker: rune::Waker) {
    let _ = WAKER.set(waker.clone());
    FIRST_ENTRIES.set(s.entries.len());
    bench_target(Some(s.cwd.clone()));
    s.setup(waker);
}

pub fn on_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if benching() {
        return true; // only the bench's own keys
    }
    let t = tx.input_time().unwrap_or_else(Instant::now);
    keyed(s, ev, tx, t)
}

fn keyed(s: &mut State, ev: &KeyEvent, tx: &mut Tx, t: Instant) -> bool {
    let asked = s.pending_path().map(Path::to_path_buf);
    let handled = s.on_key(ev, tx);
    if handled {
        KEY.set(Some(t));
        if s.pending_path().is_some_and(|p| Some(p) != asked.as_deref()) {
            NAV.set(Some(t));
        }
    }
    handled
}

pub fn poll(s: &mut State, _: f64, tx: &mut Tx) -> bool {
    let loading = s.is_loading();
    // A navigation a click (or anything but a key) asked for: the input that caused it.
    if loading && NAV.get().is_none() {
        NAV.set(Some(rune::last_input().unwrap_or_else(Instant::now)));
    }
    let mut changed = s.poll(tx);
    if loading && !s.is_loading() {
        LANDED.set(NAV.take().map(|t| (t, s.entries.len())));
    }
    // One step per frame: a frame that landed a listing is that step's frame.
    if !loading && !FIRST.get() {
        changed |= bench_step(s, tx);
    }
    changed
}

/// The next bench step, once the last one's frame is up.
fn bench_step(s: &mut State, tx: &mut Tx) -> bool {
    let (left, n) = BENCH.get();
    if left == 0 {
        if n > 0 {
            tx.quit();
        }
        return false;
    }
    BENCH.set((left - 1, n + 1));
    let now = Instant::now();
    match n % 3 {
        0 => {
            let target = bench_target(None);
            s.go(target.parent().unwrap_or(&target).to_path_buf(), Nav::New, None);
            NAV.set(Some(now));
        }
        1 => {
            s.go(bench_target(None), Nav::New, None);
            NAV.set(Some(now));
        }
        _ => {
            keyed(s, &KeyEvent { key: Key::Down, mods: Mods::default() }, tx, now);
        }
    }
    true
}

/// The folder the bench started in.
fn bench_target(first: Option<PathBuf>) -> PathBuf {
    static TARGET: OnceLock<PathBuf> = OnceLock::new();
    TARGET.get_or_init(|| first.unwrap_or_default()).clone()
}

/// Rune's per-frame report.
pub fn frame(f: &FrameEncoded) {
    // Rune runs a frame's input handlers and `poll` before building it, so whatever they marked
    // belongs to this frame, drawn or not.
    let (key, landed) = (KEY.take(), LANDED.take());
    if f.outcome() != FrameOutcome::Drawn {
        return;
    }
    let at = f.at();
    let ms = |t: Instant| at.saturating_duration_since(t).as_secs_f64() * 1000.0;
    let split = format!(
        "update_ms={:.2} encode_ms={:.2} refresh_hz={}",
        f.update_ms().unwrap_or(0.0),
        f.encode_ms().unwrap_or(0.0),
        f.refresh_hz().map_or("?".into(), |hz| format!("{hz:.0}"))
    );
    if FIRST.replace(false) {
        let (main, wall) = MAIN.get().copied().unwrap_or((at, SystemTime::now()));
        let epoch = (wall + at.saturating_duration_since(main)).duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros());
        let (init, init_at, n) = (INIT_MS.get(), INIT_AT_MS.get(), FIRST_ENTRIES.get());
        eprintln!(
            "arbor-timing first-frame main_ms={:.2} init_at_ms={init_at:.2} init_ms={init:.2} epoch_us={epoch} entries={n} {split}",
            ms(main)
        );
    } else {
        if let Some(t) = key {
            eprintln!("arbor-timing key ms={:.2} {split}", ms(t));
        }
        if let Some((t, n)) = landed {
            eprintln!("arbor-timing nav ms={:.2} entries={n} {split}", ms(t));
        }
    }
    if benching()
        && let Some(w) = WAKER.get()
    {
        w();
    }
}
