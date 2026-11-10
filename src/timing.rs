//! Opt-in measurement of the speed budgets in NORTHSTAR.md.
//!
//! `ARBOR_TIMING=1` logs to stderr, one line per event:
//!
//! ```text
//! arbor-timing first-frame main_ms=41.2 init_at_ms=3.1 init_ms=12.0 epoch_us=1762591234567890 entries=10000
//! arbor-timing key ms=1.9 update_ms=1.1 encode_ms=0.6 refresh_hz=120
//! arbor-timing nav ms=9.4 update_ms=3.2 encode_ms=0.7 entries=10000
//! arbor-timing tab ms=12.1 update_ms=3.0 encode_ms=0.7 entries=10000 same_frame=1
//! ```
//!
//! - `first-frame`: from the top of `main` to the first frame submitted. `epoch_us` is that
//!   moment on the wall clock, so a launcher can measure from before `exec` (`scripts/bench.sh`).
//!   `init_ms` is the part spent in `State::new`, which reads the first folder; `init_at_ms` is
//!   when that started (Rune sets up its event loop first, then creates the window and GPU).
//! - `key`: from the key's arrival (the OS event's own timestamp on macOS) to the frame it
//!   changed being submitted.
//! - `nav`: from the request (the key or click that asked) to the frame showing the new listing.
//! - `tab`: a key that showed a hidden tab, to the frame showing its fresh listing.
//!   `same_frame=1` when the read landed within the key's bounded wait (`State::show_tab`), so
//!   the key's own frame shows it.
//! - `preview`: a key that started a quick preview read (Space opening it, or moving the
//!   selection while it's open), to the frame showing what it read. `from=open` or `from=move`,
//!   and `kind=text|image|card|error`. A `key` line has `preview=1` when the preview was open.
//! - `jump`: the key that opened Jump to Folder, to the frame showing its ranked folders.
//!   `cold=1` when that open read the history log (the first of a session), `folders=N` the
//!   history's size, `same_frame=1` when the load landed within the open's bounded wait.
//! - `search-first` and `search-done`: the key that started a search walk (⌘F's first letter,
//!   or any change to the query), to the frame showing its first results, and to the frame
//!   after it finished (`stopped=1` when Esc ended it), with `found=N` and `visited=N`. A `key`
//!   line has `search=1` when a walk was streaming into the frame it drew.
//! - `rename`: a key that changed Batch Rename's rule, to the frame showing the preview for it,
//!   with `items=N`, `same_frame=1` when it was worked out inside the key (small batches) rather
//!   than on a worker, and `regex=1` in regex mode.
//! - `thumb-first` and `thumb-screen`: As Icons, to the frame showing the first thumbnail, and to
//!   the frame showing the last one the screen asked for (`thumbs=N` of them). A `key` line has
//!   `thumbs=N` when N thumbnails were waiting or decoding as it was handled.
//! - `first-listing`: only when the first folder was too slow for the first frame (the window
//!   waits at most `app::FIRST_PAINT_WAIT` for it): from the top of `main` to the frame showing
//!   it. `ARBOR_SLOW_FIRST_READ_MS=N` (read only when timing is on) slows that read by N ms to
//!   try it.
//!
//! "Submitted" is when Rune handed the frame to the GPU (`FrameEncoded::at`): a proxy for
//! "pixels on screen", which come at least one refresh later (Rune's own presentation timing,
//! `frame/present_time.rs`, runs two to three frames behind). Rune doesn't report when a frame
//! reached the screen (see RUNE-GAPS.md).
//!
//! `ARBOR_BENCH=N` (implies timing) drives the app itself: after the first frame it does N
//! rounds of "go to the parent, come back, press ↓", each step started by the previous step's
//! frame (no timers), then quits. With `ARBOR_BENCH_PREVIEW=1` (in a folder whose first two
//! files are a text file and an image) each round is: Space (preview the text), Space, ↓,
//! Space (preview the image), ↑, ↓, ↑ (the last two while it's open, the ↑ while the image
//! decodes), Space; opening waits for the preview to land. With `ARBOR_BENCH_JUMP=1` each
//! round opens Jump to Folder (waiting for the history to land; the first open of a run reads
//! and aggregates the log, the rest rank what's in memory), types two letters (a different
//! pair each round, from [`JUMP_QUERIES`], so no round repeats the last one's rows), and closes
//! it with Esc. With `ARBOR_BENCH_SEARCH=1` each round opens Search in This Folder, types `1`
//! (in a large tree, so the walk streams for many frames), presses ↓ once a frame while it
//! streams, and leaves with Esc once it's done. With `ARBOR_BENCH_RENAME=1` each round selects
//! everything, opens Batch Rename (waiting for the folder's read), types `e`, turns on regex,
//! types `$` (each waiting for its preview) and closes it with Esc. With `ARBOR_BENCH_GRID=1` the
//! first step switches to the icon view (As Icons); then each round presses Page Down and ↓.
//! `ARBOR_BENCH_GRID=2` (a folder of photos) waits after As Icons for the first screen's
//! thumbnails, then pages while the next screens' stream in. With `ARBOR_BENCH_TABS=1` it opens a second tab on the
//! parent folder instead, then does N rounds of "previous tab (the start folder), next tab".
//! A step starts only once the previous step's frame was drawn: a frame that wasn't (the window
//! covered or not yet visible, no drawable in time) asks for nothing, and Rune draws again when
//! it can. (A wake from an undrawn frame was handled in the same run-loop turn, so AppKit never
//! got to say the window was visible, and the bench spun: RUNE-GAPS.md.)
//! `ARBOR_BENCH_PACE_MS=N` spaces the steps at least N ms apart, as a person types (a thread
//! sleeps and wakes the window; nothing polls), so the CPU cools between keys as it does for a
//! real user. The ↓ is synthetic, so its `key` time starts when the handler
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
use crate::commands::Cmd;

/// Runs `cmd` as its key would. True when it could run.
fn run(s: &mut State, cmd: Cmd, tx: &mut Tx) -> bool {
    if s.unavailable(cmd).is_some() {
        return false;
    }
    s.run(cmd, tx);
    true
}

/// When `main` started, on both clocks.
static MAIN: OnceLock<(Instant, SystemTime)> = OnceLock::new();
/// Wakes the window between bench steps.
static WAKER: OnceLock<rune::Waker> = OnceLock::new();

thread_local! {
    /// A handled key waiting for its frame: when it arrived.
    static KEY: Cell<Option<Instant>> = const { Cell::new(None) };
    /// The navigation in flight: when it was asked for, and whether a tab switch asked.
    static NAV: Cell<Option<(Instant, Kind)>> = const { Cell::new(None) };
    /// A navigation whose listing lands in the frame being built: when it was asked for, how
    /// many entries it has, what asked, and whether it landed inside the key.
    static LANDED: Cell<Option<(Instant, usize, Kind, bool)>> = const { Cell::new(None) };
    /// `ARBOR_BENCH_TABS`: the bench switches tabs instead of navigating.
    static TABS: Cell<bool> = const { Cell::new(false) };
    /// `ARBOR_BENCH_PREVIEW`: the bench opens and moves the quick preview.
    static PREVIEW: Cell<bool> = const { Cell::new(false) };
    /// A preview read in flight: when its key came, and whether that key opened the preview.
    static PREVIEW_ASKED: Cell<Option<(Instant, bool)>> = const { Cell::new(None) };
    /// A preview that lands in the frame being built: its key's time, whether it opened, its kind.
    static PREVIEW_LANDED: Cell<Option<(Instant, bool, &'static str)>> = const { Cell::new(None) };
    /// The handled key's frame shows an open preview.
    static KEY_PREVIEW: Cell<bool> = const { Cell::new(false) };
    /// `ARBOR_BENCH_JUMP`: the bench opens Jump to Folder and types in it.
    static JUMP: Cell<bool> = const { Cell::new(false) };
    /// `ARBOR_BENCH_SEARCH`: the bench searches and moves through the results as they stream.
    static SEARCH: Cell<bool> = const { Cell::new(false) };
    /// Where the search bench is in its round: ⌘F, the query, ↓ while it streams.
    static SEARCH_PHASE: Cell<u8> = const { Cell::new(0) };
    /// The handled key's frame has a walk streaming into it.
    static KEY_SEARCH: Cell<bool> = const { Cell::new(false) };
    /// A search walk in flight: the key that started it, and whether its first results showed.
    static SEARCH_ASKED: Cell<Option<(Instant, bool)>> = const { Cell::new(None) };
    /// The search's first results land in the frame being built: its key, and how many.
    static SEARCH_FIRST: Cell<Option<(Instant, usize)>> = const { Cell::new(None) };
    /// The search finished (or stopped) in the frame being built: its key, found, visited, stopped.
    static SEARCH_DONE: Cell<Option<(Instant, usize, u64, bool)>> = const { Cell::new(None) };
    /// Jump opened and its history is still loading: when, and whether it reads the log.
    static JUMP_ASKED: Cell<Option<(Instant, bool)>> = const { Cell::new(None) };
    /// Jump's rows land in the frame being built: the open's time, cold, folders, same frame.
    static JUMP_LANDED: Cell<Option<(Instant, bool, usize, bool)>> = const { Cell::new(None) };
    /// `ARBOR_BENCH_RENAME`: the bench selects everything, opens Batch Rename and types in it.
    static RENAME: Cell<bool> = const { Cell::new(false) };
    /// `ARBOR_BENCH_GRID`: the bench switches to the icon view and pages through it; `=2` in a
    /// folder of photos, whose first screen of thumbnails it waits for.
    static GRID: Cell<bool> = const { Cell::new(false) };
    static GRID_PHOTOS: Cell<bool> = const { Cell::new(false) };
    /// As Icons asked for thumbnails: when, and whether the first one showed yet.
    static TILE_ASKED: Cell<Option<(Instant, bool)>> = const { Cell::new(None) };
    /// The first thumbnail, and the screen's last, land in the frame being built.
    static TILE_FIRST: Cell<Option<Instant>> = const { Cell::new(None) };
    static TILE_DONE: Cell<Option<(Instant, usize)>> = const { Cell::new(None) };
    /// Thumbnails waiting or decoding when the handled key came.
    static KEY_THUMBS: Cell<usize> = const { Cell::new(0) };
    /// A Batch Rename plan being worked out on a worker: the key that changed the rule.
    static RENAME_ASKED: Cell<Option<Instant>> = const { Cell::new(None) };
    /// A Batch Rename plan lands in the frame being built: its key, items, same frame, regex.
    static RENAME_LANDED: Cell<Option<(Instant, usize, bool, bool)>> = const { Cell::new(None) };
    static FIRST: Cell<bool> = const { Cell::new(true) };
    /// Entries in the first listing.
    static FIRST_ENTRIES: Cell<usize> = const { Cell::new(0) };
    /// The first listing missed the first frame and is still on its way.
    static BOOT_LATE: Cell<bool> = const { Cell::new(false) };
    /// How long `State::new` took (it waits a bounded time for the first folder).
    static INIT_MS: Cell<f64> = const { Cell::new(0.0) };
    /// When `State::new` started, from the top of `main` (Rune's event loop setup before it).
    static INIT_AT_MS: Cell<f64> = const { Cell::new(0.0) };
    /// Bench steps left, and the step counter.
    static BENCH: Cell<(u32, u32)> = const { Cell::new((0, 0)) };
    /// The last preview bench step waits for its preview to land.
    static WAIT_PREVIEW: Cell<bool> = const { Cell::new(false) };
    /// The last bench step's frame was drawn: the next may start.
    static STEP_SHOWN: Cell<bool> = const { Cell::new(true) };
    /// `ARBOR_BENCH_PACE_MS`, the least time between steps, and when the last one started.
    static PACE_MS: Cell<u64> = const { Cell::new(0) };
    static LAST_STEP: Cell<Option<Instant>> = const { Cell::new(None) };
    /// A pacing thread is asleep and will wake the window.
    static PACE_ARMED: Cell<bool> = const { Cell::new(false) };
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
    TABS.set(set("ARBOR_BENCH_TABS"));
    PREVIEW.set(set("ARBOR_BENCH_PREVIEW"));
    JUMP.set(set("ARBOR_BENCH_JUMP"));
    SEARCH.set(set("ARBOR_BENCH_SEARCH"));
    RENAME.set(set("ARBOR_BENCH_RENAME"));
    GRID.set(set("ARBOR_BENCH_GRID"));
    GRID_PHOTOS.set(std::env::var("ARBOR_BENCH_GRID").is_ok_and(|v| v.trim() == "2"));
    PACE_MS.set(std::env::var("ARBOR_BENCH_PACE_MS").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0));
    let steps = if SEARCH.get() {
        rounds
    } else if RENAME.get() {
        rounds * RENAME_STEPS
    } else if PREVIEW.get() {
        rounds * PREVIEW_STEPS
    } else if JUMP.get() {
        rounds * JUMP_STEPS
    } else if TABS.get() || GRID.get() {
        rounds * 2 + 1
    } else {
        rounds * 3
    };
    BENCH.set((steps, 0));
    let slow = std::env::var("ARBOR_SLOW_FIRST_READ_MS").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
    crate::app::SLOW_FIRST_READ_MS.store(slow, std::sync::atomic::Ordering::Relaxed);
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
    FIRST_ENTRIES.set(s.cur().entries.len());
    if s.is_loading() {
        // Timed from `main`, and reported as `first-listing` rather than as a navigation.
        BOOT_LATE.set(true);
        NAV.set(MAIN.get().map(|(m, _)| (*m, Kind::Nav)));
    }
    bench_target(Some(s.cur().cwd.clone()));
    s.setup(waker);
}

pub fn on_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if benching() {
        return true; // only the bench's own keys
    }
    let t = tx.input_time().unwrap_or_else(Instant::now);
    keyed(s, ev, tx, t)
}

/// A key that reached a focused element owning its keys (the file list), going where the
/// window's own `on_key` sends it: timed when timing is on.
pub fn key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if MAIN.get().is_some() { on_key(s, ev, tx) } else { s.on_key(ev, tx) }
}

fn keyed(s: &mut State, ev: &KeyEvent, tx: &mut Tx, t: Instant) -> bool {
    measured(s, tx, t, |s, tx| s.on_key(ev, tx))
}

/// What asked for a listing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Nav,
    Tab,
}

/// Runs `f` (a key's handling) and notes what it asked for: a key frame, and a listing that
/// landed inside it or is now on its way.
fn measured(s: &mut State, tx: &mut Tx, t: Instant, f: impl FnOnce(&mut State, &mut Tx) -> bool) -> bool {
    let asked = s.pending_path().map(Path::to_path_buf);
    let (lands, tab) = (s.lands, tab_of(s));
    let (reads, closed) = (s.preview_reads, s.preview.is_none());
    let (no_jump, cold) = (s.jump.is_none(), s.history.loads == 0);
    let generation = search_generation(s);
    let (had_batch, plans) = (s.batch.is_some(), s.batch_plans);
    let handled = f(s, tx);
    if handled
        && had_batch
        && let Some(b) = &s.batch
    {
        if b.stale() {
            RENAME_ASKED.set(Some(t));
        } else if s.batch_plans != plans {
            RENAME_LANDED.set(Some((t, b.items.len(), true, b.regex)));
        }
    }
    if handled && search_generation(s) != generation && searching(s) {
        SEARCH_ASKED.set(Some((t, false)));
    }
    if handled
        && no_jump
        && let Some(j) = &s.jump
    {
        let folders = s.history.agg.as_ref().map_or(0, |a| a.len());
        if j.loading {
            JUMP_ASKED.set(Some((t, cold)));
        } else {
            JUMP_LANDED.set(Some((t, cold, folders, true)));
        }
    }
    if handled {
        KEY.set(Some(t));
        KEY_THUMBS.set(s.cur().tiles.as_ref().map_or(0, crate::tiles::Tiles::waiting));
        KEY_PREVIEW.set(s.preview.is_some());
        KEY_SEARCH.set(searching(s));
        if s.preview_reads != reads {
            PREVIEW_ASKED.set(Some((t, closed)));
        }
        let kind = if tab_of(s) != tab { Kind::Tab } else { Kind::Nav };
        if s.lands != lands && !s.is_loading() {
            LANDED.set(Some((t, s.cur().entries.len(), kind, true)));
        } else if s.pending_path().is_some_and(|p| Some(p) != asked.as_deref()) {
            NAV.set(Some((t, kind)));
        }
    }
    handled
}

/// The active pane's search walk, if any (`None` when it isn't searching).
fn search_generation(s: &State) -> Option<u64> {
    s.cur().search.as_ref().map(|x| x.generation)
}

/// A search walk is running in the active pane.
fn searching(s: &State) -> bool {
    s.cur().search.as_ref().is_some_and(|x| x.walking())
}

/// The active pane and its visible tab.
fn tab_of(s: &State) -> (u64, usize) {
    let p = &s.panes[s.active];
    (p.id(), p.tab)
}

pub fn poll(s: &mut State, _: f64, tx: &mut Tx) -> bool {
    let loading = s.is_loading();
    // A navigation a click (or anything but a key) asked for: the input that caused it.
    if loading && NAV.get().is_none() {
        NAV.set(Some((rune::last_input().unwrap_or_else(Instant::now), Kind::Nav)));
    }
    let previews = s.preview_lands;
    let jump_loads = s.history.loads;
    let plans = s.batch_plans;
    let mut changed = s.poll(tx);
    if let Some((t, seen)) = TILE_ASKED.get()
        && let Some(tiles) = s.cur().tiles.as_ref()
    {
        if !seen && !tiles.shown.is_empty() {
            TILE_FIRST.set(Some(t));
            TILE_ASKED.set(Some((t, true)));
        }
        if tiles.waiting() == 0 && !tiles.shown.is_empty() {
            TILE_DONE.set(Some((t, tiles.shown.len())));
            TILE_ASKED.set(None);
        }
    }
    if s.batch_plans != plans
        && let Some(b) = s.batch.as_ref().filter(|b| !b.stale())
        && let Some(t) = RENAME_ASKED.take()
    {
        RENAME_LANDED.set(Some((t, b.items.len(), false, b.regex)));
    }
    if let Some((t, seen)) = SEARCH_ASKED.get() {
        let f = s.cur();
        let found = f.entries.len();
        if !seen && found > 0 && f.searching() {
            SEARCH_FIRST.set(Some((t, found)));
            SEARCH_ASKED.set(Some((t, true)));
        }
        if let Some(x) = f.search.as_ref().filter(|x| !x.walking()) {
            SEARCH_DONE.set(Some((t, found, x.visited, x.stopped)));
            SEARCH_ASKED.set(None);
        } else if !f.searching() {
            SEARCH_ASKED.set(None);
        }
    }
    if s.history.loads != jump_loads
        && let Some((t, cold)) = JUMP_ASKED.take()
    {
        JUMP_LANDED.set(Some((t, cold, s.history.agg.as_ref().map_or(0, |a| a.len()), false)));
    }
    if loading && !s.is_loading() {
        LANDED.set(NAV.take().map(|(t, kind)| (t, s.cur().entries.len(), kind, false)));
    }
    if s.preview_lands != previews
        && let Some((t, opened)) = PREVIEW_ASKED.take()
    {
        PREVIEW_LANDED.set(Some((t, opened, preview_kind(s))));
    }
    // One step per drawn frame: a frame that landed a listing, a preview or Jump's history is
    // that step's frame, and a step whose frame wasn't drawn yet waits for it.
    if !loading && !FIRST.get() && s.preview_lands == previews && s.history.loads == jump_loads && STEP_SHOWN.get() && paced() {
        let stepped = bench_step(s, tx);
        if stepped {
            STEP_SHOWN.set(false);
            LAST_STEP.set(Some(Instant::now()));
        }
        changed |= stepped;
    }
    changed
}

/// Whether `ARBOR_BENCH_PACE_MS` has passed since the last step. If not, a thread sleeps out
/// the rest and wakes the window (once).
fn paced() -> bool {
    let pace = std::time::Duration::from_millis(PACE_MS.get());
    let Some(last) = LAST_STEP.get().filter(|_| !pace.is_zero()) else { return true };
    let since = last.elapsed();
    if since >= pace {
        PACE_ARMED.set(false);
        return true;
    }
    if !PACE_ARMED.replace(true)
        && let Some(w) = WAKER.get().cloned()
    {
        let rest = pace - since;
        std::thread::spawn(move || {
            std::thread::sleep(rest);
            w();
        });
    }
    false
}

fn preview_kind(s: &State) -> &'static str {
    use crate::preview::Body;
    match s.preview.as_ref().map(|p| &p.body) {
        Some(Body::Text(_)) => "text",
        Some(Body::Image(_)) => "image",
        Some(Body::Card(_)) => "card",
        Some(Body::Error(_)) => "error",
        _ => "none",
    }
}

/// Steps in a preview bench round (see the module docs).
const PREVIEW_STEPS: u32 = 8;
/// Steps in a Jump bench round: open, two letters, Esc.
const JUMP_STEPS: u32 = 4;
/// What the Jump bench types, one pair a round: each matches a different part of its history,
/// so the rows change (and are laid out afresh) every round, as they do for a person.
pub const JUMP_QUERIES: [&str; 12] = ["ar", "do", "pr", "cl", "re", "no", "mu", "sr", "ph", "de", "li", "bu"];
/// Steps in a Batch Rename bench round: select all, open, `e`, regex on, `$`, Esc.
const RENAME_STEPS: u32 = 6;

/// One step of the preview bench: the keys, and whether to wait for the preview to land first.
fn preview_step(s: &mut State, tx: &mut Tx, n: u32, now: Instant) -> bool {
    let space = || KeyEvent { key: Key::Text(" ".into()), mods: Mods::default() };
    let (key, then_wait) = match n % PREVIEW_STEPS {
        0 => (space(), true),
        1 => (space(), false),
        2 => (KeyEvent { key: Key::Down, mods: Mods::default() }, false),
        3 => (space(), true),
        4 => (KeyEvent { key: Key::Up, mods: Mods::default() }, false),
        5 => (KeyEvent { key: Key::Down, mods: Mods::default() }, false),
        6 => (KeyEvent { key: Key::Up, mods: Mods::default() }, true),
        _ => (space(), false),
    };
    keyed(s, &key, tx, now);
    then_wait
}

/// The next bench step, once the last one's frame is up.
fn bench_step(s: &mut State, tx: &mut Tx) -> bool {
    let (left, n) = BENCH.get();
    // The preview bench waits for a preview to land before its next step.
    if PREVIEW.get() && WAIT_PREVIEW.get() && s.preview.as_ref().is_some_and(|p| p.is_loading()) {
        return false;
    }
    if left == 0 {
        if n > 0 {
            tx.quit();
        }
        return false;
    }
    // The Jump bench waits for the history to land before typing, and the Batch Rename bench
    // for the folder's read and each plan.
    if JUMP.get() && s.jump.as_ref().is_some_and(|j| j.loading) {
        return false;
    }
    if RENAME.get() && s.batch.as_ref().is_some_and(|b| !b.known() || b.stale()) {
        return false;
    }
    // The grid bench lets the first screen's thumbnails land before paging (that's the
    // time to the first thumbnail); later pages stream theirs while the keys go on.
    if GRID_PHOTOS.get() && n == 1 && TILE_ASKED.get().is_some() {
        return false;
    }
    let now = Instant::now();
    if SEARCH.get() {
        let key = |k: Key| KeyEvent { key: k, mods: Mods::default() };
        match SEARCH_PHASE.get() {
            0 => {
                measured(s, tx, now, |s, tx| run(s, Cmd::Search, tx));
                SEARCH_PHASE.set(1);
            }
            1 => {
                keyed(s, &key(Key::Text("1".into())), tx, now);
                SEARCH_PHASE.set(2);
            }
            _ if searching(s) => {
                keyed(s, &key(Key::Down), tx, now);
            }
            _ => {
                keyed(s, &key(Key::Escape), tx, now);
                SEARCH_PHASE.set(0);
                BENCH.set((left - 1, n + 1));
            }
        }
        return true;
    }
    BENCH.set((left - 1, n + 1));
    if RENAME.get() {
        let key = |k: Key| KeyEvent { key: k, mods: Mods::default() };
        match n % RENAME_STEPS {
            0 => {
                measured(s, tx, now, |s, tx| run(s, Cmd::SelectAll, tx));
            }
            1 => {
                s.batch_rule = crate::rename::Rule::default();
                measured(s, tx, now, |s, tx| run(s, Cmd::BatchRename, tx));
            }
            2 => {
                keyed(s, &key(Key::Text("e".into())), tx, now);
            }
            3 => {
                // ⌥R, as the field would take it.
                measured(s, tx, now, |s, _| {
                    s.batch_set(|b| b.regex = true);
                    true
                });
            }
            4 => {
                keyed(s, &key(Key::Text("$".into())), tx, now);
            }
            _ => {
                keyed(s, &key(Key::Escape), tx, now);
            }
        }
        return true;
    }
    if JUMP.get() {
        let key = |k: Key| KeyEvent { key: k, mods: Mods::default() };
        match n % JUMP_STEPS {
            0 => {
                measured(s, tx, now, |s, tx| run(s, Cmd::Jump, tx));
            }
            k @ (1 | 2) => {
                let q = JUMP_QUERIES[(n / JUMP_STEPS) as usize % JUMP_QUERIES.len()];
                let c = q[k as usize - 1..k as usize].to_string();
                keyed(s, &key(Key::Text(c)), tx, now);
            }
            _ => {
                keyed(s, &key(Key::Escape), tx, now);
            }
        }
        return true;
    }
    if PREVIEW.get() {
        WAIT_PREVIEW.set(preview_step(s, tx, n, now));
        return true;
    }
    if GRID.get() {
        let key = |k: Key| KeyEvent { key: k, mods: Mods::default() };
        match n {
            0 => {
                measured(s, tx, now, |s, tx| run(s, Cmd::AsIcons, tx));
                TILE_ASKED.set(Some((now, false)));
            }
            n if n % 2 == 1 => {
                keyed(s, &key(Key::PageDown), tx, now);
            }
            _ => {
                keyed(s, &key(Key::Down), tx, now);
            }
        }
        return true;
    }
    if TABS.get() {
        match n {
            // A second tab on the parent folder.
            0 => {
                s.run(Cmd::NewTab, tx);
                let target = bench_target(None);
                s.go(target.parent().unwrap_or(&target).to_path_buf(), Nav::New, None);
                NAV.set(Some((now, Kind::Nav)));
            }
            n if n % 2 == 1 => {
                measured(s, tx, now, |s, tx| run(s, Cmd::PrevTab, tx));
            }
            _ => {
                measured(s, tx, now, |s, tx| run(s, Cmd::NextTab, tx));
            }
        }
        return true;
    }
    match n % 3 {
        0 => {
            let target = bench_target(None);
            s.go(target.parent().unwrap_or(&target).to_path_buf(), Nav::New, None);
            NAV.set(Some((now, Kind::Nav)));
        }
        1 => {
            s.go(bench_target(None), Nav::New, None);
            NAV.set(Some((now, Kind::Nav)));
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

/// What input handlers and `poll` marked for the frame that shows it.
struct Marked {
    key: Option<Instant>,
    landed: Option<(Instant, usize, Kind, bool)>,
    preview: Option<(Instant, bool, &'static str)>,
    jump: Option<(Instant, bool, usize, bool)>,
    search_first: Option<(Instant, usize)>,
    search_done: Option<(Instant, usize, u64, bool)>,
    rename: Option<(Instant, usize, bool, bool)>,
    tile_first: Option<Instant>,
    tile_done: Option<(Instant, usize)>,
}

/// What was marked since the last drawn frame, taken only if this frame was `drawn`. Rune runs
/// a frame's input handlers and `poll` before building it, so what they marked shows in this
/// frame if it's drawn; one that isn't (`Skipped`: no drawable came in time, as when a long
/// build loads a font; `Unchanged`) shows nothing, so what it marked waits for the next drawn
/// frame, which is the one that shows it. (Taking it anyway lost the event: a preview that
/// landed in a skipped frame was never logged, and the bench counted one fewer.)
fn take_marked(drawn: bool) -> Option<Marked> {
    if !drawn {
        return None;
    }
    Some(Marked {
        key: KEY.take(),
        landed: LANDED.take(),
        preview: PREVIEW_LANDED.take(),
        jump: JUMP_LANDED.take(),
        search_first: SEARCH_FIRST.take(),
        search_done: SEARCH_DONE.take(),
        rename: RENAME_LANDED.take(),
        tile_first: TILE_FIRST.take(),
        tile_done: TILE_DONE.take(),
    })
}

/// Rune's per-frame report.
pub fn frame(f: &FrameEncoded) {
    let drawn = f.outcome() == FrameOutcome::Drawn;
    let wake = shown(drawn);
    let Some(Marked { key, landed, preview, jump, search_first, search_done, rename, tile_first, tile_done }) = take_marked(drawn) else {
        return;
    };
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
        let (init, init_at) = (INIT_MS.get(), INIT_AT_MS.get());
        // A late first listing can still land in time for the first frame's `poll`.
        let n = match landed {
            Some((_, n, _, _)) if BOOT_LATE.replace(false) => n,
            _ => FIRST_ENTRIES.get(),
        };
        eprintln!(
            "arbor-timing first-frame main_ms={:.2} init_at_ms={init_at:.2} init_ms={init:.2} epoch_us={epoch} entries={n} {split}",
            ms(main)
        );
    } else {
        if let Some(t) = key {
            eprintln!(
                "arbor-timing key ms={:.2} {split} preview={} search={} thumbs={}",
                ms(t),
                u8::from(KEY_PREVIEW.get()),
                u8::from(KEY_SEARCH.get()),
                KEY_THUMBS.get()
            );
        }
        if let Some(t) = tile_first {
            eprintln!("arbor-timing thumb-first ms={:.2} {split}", ms(t));
        }
        if let Some((t, n)) = tile_done {
            eprintln!("arbor-timing thumb-screen ms={:.2} thumbs={n} {split}", ms(t));
        }
        if let Some((t, found)) = search_first {
            eprintln!("arbor-timing search-first ms={:.2} found={found} {split}", ms(t));
        }
        if let Some((t, found, visited, stopped)) = search_done {
            eprintln!("arbor-timing search-done ms={:.2} found={found} visited={visited} stopped={} {split}", ms(t), u8::from(stopped));
        }
        if let Some((t, cold, folders, inside)) = jump {
            eprintln!(
                "arbor-timing jump ms={:.2} cold={} folders={folders} same_frame={} {split}",
                ms(t),
                u8::from(cold),
                u8::from(inside)
            );
        }
        if let Some((t, items, inside, regex)) = rename {
            eprintln!(
                "arbor-timing rename ms={:.2} items={items} same_frame={} regex={} {split}",
                ms(t),
                u8::from(inside),
                u8::from(regex)
            );
        }
        if let Some((t, opened, kind)) = preview {
            eprintln!("arbor-timing preview ms={:.2} from={} kind={kind} {split}", ms(t), if opened { "open" } else { "move" });
        }
        if let Some((t, n, kind, inside)) = landed {
            if BOOT_LATE.replace(false) {
                eprintln!("arbor-timing first-listing main_ms={:.2} entries={n} {split}", ms(t));
            } else if kind == Kind::Tab {
                eprintln!("arbor-timing tab ms={:.2} entries={n} {split} same_frame={}", ms(t), u8::from(inside));
            } else {
                eprintln!("arbor-timing nav ms={:.2} entries={n} {split}", ms(t));
            }
        }
    }
    if wake && let Some(w) = WAKER.get() {
        w();
    }
}

/// A frame's outcome for the bench: a drawn one showed the last step, so the next may start,
/// and the bench wakes for it. One that wasn't drawn does neither: Rune draws again when it can
/// (the window shows, a drawable comes), and that frame is the step's. A wake from an undrawn
/// frame was handled in the same run-loop turn, so a covered window spun without AppKit ever
/// reporting it visible (RUNE-GAPS.md).
fn shown(drawn: bool) -> bool {
    if drawn {
        STEP_SHOWN.set(true);
    }
    drawn && benching()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a drawn frame lets the bench's next step start; an undrawn one asks for nothing.
    #[test]
    fn the_next_step_waits_for_a_drawn_frame() {
        STEP_SHOWN.set(false);
        assert!(!shown(false), "no wake from an undrawn frame");
        assert!(!STEP_SHOWN.get());
        shown(true);
        assert!(STEP_SHOWN.get());
    }

    /// Paced steps wait out the gap after the last one.
    #[test]
    fn paced_steps_wait_out_the_gap() {
        PACE_MS.set(0);
        LAST_STEP.set(Some(Instant::now()));
        assert!(paced(), "unpaced");
        PACE_MS.set(60_000);
        assert!(!paced(), "too soon");
        assert!(PACE_ARMED.get());
        LAST_STEP.set(Instant::now().checked_sub(std::time::Duration::from_secs(61)));
        assert!(paced());
        assert!(!PACE_ARMED.get());
        PACE_MS.set(0);
    }

    /// A preview (or anything else) that lands in a frame that isn't drawn is reported with
    /// the next frame that is, timed to it, never dropped.
    #[test]
    fn what_lands_in_a_skipped_frame_waits_for_the_next_drawn_one() {
        let t = Instant::now();
        PREVIEW_LANDED.set(Some((t, true, "text")));
        KEY.set(Some(t));
        assert!(take_marked(false).is_none(), "a skipped frame shows nothing");
        let m = take_marked(true).expect("the next drawn frame");
        assert_eq!(m.preview.map(|p| p.2), Some("text"));
        assert_eq!(m.key, Some(t));
        assert!(take_marked(true).unwrap().preview.is_none(), "reported once");
    }
}
