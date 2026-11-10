//! Thumbnails in the icon view, for the cells on screen and nothing else.
//!
//! View-triggered background work, so it follows each of NORTHSTAR's rules for that:
//!
//! - **First paint never waits.** A cell shows its type icon at once (`view/grid.rs`); a
//!   thumbnail replaces it when one lands, in whatever frame that is.
//! - **Scoped to what's on screen.** Rune says which rows of the grid are visible
//!   (`on_visible_rows`), whenever that changes; only their cells are asked for, top left
//!   first. A cell that scrolls off has its request taken out of the queue, or its decode
//!   stopped (each has a stop flag, checked between reads and rows; an OS request is cancelled,
//!   its wait rung awake). Whatever takes the grid off screen (the list view, another tab, a
//!   filter that leaves nothing, the preview covering its pane, another folder, closing the
//!   window) drops it all the same way, checked after every input and landing
//!   ([`State::sync_tiles`]), never left to the vanishing grid's own callbacks.
//! - **Finishes and stops.** One decode per cell, then nothing: no watching.
//! - **Nothing is stored.** The decoded thumbnails of the cells on screen are part of the view,
//!   dropped as their cells leave the screen; scrolling back decodes again. Nothing goes to
//!   disk.
//! - **A slow disk slows thumbnails, never scrolling.** Decoding runs on a fixed pool of
//!   [`WORKERS`] threads below the window's priority (CPU and disk), one decode each, started
//!   the first time a grid has something to thumbnail and idle (asleep on a condition
//!   variable) after.
//!
//! Images use the preview's own decoders, with its caps (`preview::image_thumbnail`): shrunk to
//! the icon's size in device pixels while decoding, so memory is bounded by the cells on screen
//! times one icon's pixels (256 × 256 at the large size on a 2× display: 256 KB each). On macOS
//! (Quick Look) and Windows (the shell's cache only, as the preview) documents and video get the
//! OS's thumbnail too; Linux's thumbnailers keep a cache, so there they keep their type icon.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use rune::Pixels;

use crate::app::State;
use crate::fs::Entry;
use crate::thumb::{Bell, Rgba};

/// Threads decoding thumbnails, at most.
pub const WORKERS: usize = 2;

/// One grid's thumbnails: those decoded for the cells on screen, and those asked for.
pub struct Tiles {
    /// Decoded, by item name, with their size in device pixels: cells on screen only.
    pub shown: HashMap<OsString, (Pixels, [u32; 2])>,
    asked: HashMap<OsString, Asked>,
    /// The items (indices into the folder's visible ones) on screen when last scheduled.
    pub items: Range<usize>,
    /// The folder's version and the icon's size in device pixels they're for: either changing
    /// starts over.
    version: u64,
    fit: u32,
    /// Tells this grid's answers from any other's.
    id: u64,
    pool: Arc<Shared>,
}

enum Asked {
    Waiting(Arc<AtomicBool>),
    /// Decoded to nothing (not an image Arbor reads, too large, no OS thumbnail): its type icon
    /// stays, and it isn't asked again while it's on screen.
    Nothing,
}

impl Drop for Tiles {
    fn drop(&mut self) {
        let mut any = false;
        for a in self.asked.values() {
            if let Asked::Waiting(stop) = a {
                stop.store(true, Relaxed);
                any = true;
            }
        }
        if any {
            self.pool.cancelled();
        }
    }
}

impl Tiles {
    /// The icon's side in device pixels these are decoded for.
    #[cfg(test)]
    pub fn fit(&self) -> u32 {
        self.fit
    }

    /// How many requests are waiting or being decoded (tests and timing).
    pub fn waiting(&self) -> usize {
        self.asked.values().filter(|a| matches!(a, Asked::Waiting(_))).count()
    }
}

/// What a worker decodes.
pub(crate) struct Job {
    grid: u64,
    name: OsString,
    pub path: PathBuf,
    /// Through the OS's thumbnailer rather than Arbor's decoders.
    os: bool,
    fit: u32,
    stop: Arc<AtomicBool>,
}

struct Done {
    grid: u64,
    name: OsString,
    rgba: Option<Rgba>,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    done: Vec<Done>,
    waker: Option<rune::Waker>,
    closed: bool,
    /// Decodes running now, the most ever at once, and how many started (tests, timing).
    running: usize,
    most: usize,
    started: usize,
}

struct Shared {
    q: Mutex<Queue>,
    cv: Condvar,
    /// One per worker, rung on every cancel, so a wait on the OS's thumbnailer ends at once.
    bells: Vec<Arc<Bell>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.q.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Some requests' stop flags were set: the waiting ones leave the queue, the running ones
    /// stop at their next check.
    fn cancelled(&self) {
        self.lock().jobs.retain(|j| !j.stop.load(Relaxed));
        for b in &self.bells {
            b.ring();
        }
    }

    fn run(&self, bell: &Arc<Bell>) {
        loop {
            let job = {
                let mut q = self.lock();
                loop {
                    if q.closed {
                        return;
                    }
                    match q.jobs.pop_front() {
                        Some(j) if j.stop.load(Relaxed) => continue,
                        Some(j) => break j,
                        None => q = self.cv.wait(q).unwrap_or_else(|e| e.into_inner()),
                    }
                }
            };
            {
                let mut q = self.lock();
                q.running += 1;
                q.started += 1;
                q.most = q.most.max(q.running);
            }
            // A decoder that panics on a corrupt file leaves that cell's icon, nothing more.
            let rgba = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode(&job, bell))).unwrap_or(None);
            let mut q = self.lock();
            q.running -= 1;
            if job.stop.load(Relaxed) {
                continue;
            }
            q.done.push(Done { grid: job.grid, name: job.name, rgba });
            if let Some(w) = q.waker.clone() {
                drop(q);
                w();
            }
        }
    }
}

/// The workers, started by the first grid with something to thumbnail, ended with the state.
pub struct Pool {
    shared: Arc<Shared>,
}

impl Drop for Pool {
    fn drop(&mut self) {
        let mut q = self.shared.lock();
        q.closed = true;
        q.jobs.clear();
        drop(q);
        self.shared.cv.notify_all();
        self.shared.cancelled();
    }
}

impl Pool {
    pub fn start(waker: Option<rune::Waker>) -> Pool {
        let bells: Vec<Arc<Bell>> = (0..WORKERS).map(|_| Arc::default()).collect();
        let shared = Arc::new(Shared { q: Mutex::new(Queue { waker, ..Queue::default() }), cv: Condvar::new(), bells });
        for (n, bell) in shared.bells.iter().enumerate() {
            let (s, bell) = (shared.clone(), bell.clone());
            std::thread::Builder::new()
                .name(format!("arbor-thumb-{n}"))
                .spawn(move || {
                    // Below the window's priority (CPU and disk), as the search's walkers: a
                    // busy machine slows the thumbnails, not the keys.
                    crate::search::background();
                    s.run(&bell)
                })
                .expect("can't start a thumbnail thread");
        }
        Pool { shared }
    }

    fn submit(&self, jobs: Vec<Job>) {
        if jobs.is_empty() {
            return;
        }
        self.shared.lock().jobs.extend(jobs);
        self.shared.cv.notify_all();
    }

    fn take_done(&self) -> Vec<Done> {
        std::mem::take(&mut self.shared.lock().done)
    }

    #[cfg(test)]
    /// Requests waiting, decodes running now, the most ever at once and how many started.
    pub fn stats(&self) -> (usize, usize, usize, usize) {
        let q = self.shared.lock();
        (q.jobs.len(), q.running, q.most, q.started)
    }
}

/// Image formats Arbor's own decoders read (found by their bytes when decoded).
const OWN: [&str; 7] = ["png", "jpg", "jpeg", "gif", "webp", "bmp", "jfif"];

/// Whether `e` gets a thumbnail here: an image Arbor decodes, or on macOS and Windows anything
/// the OS's thumbnailer knows (documents, video, other image formats). By name; nothing read.
pub(crate) fn wants(e: &Entry) -> Option<bool> {
    if e.is_dir || e.size == 0 {
        return None;
    }
    let ext = crate::fs::ext(&e.lower);
    if OWN.contains(&ext) {
        return Some(false);
    }
    (cfg!(any(target_os = "macos", windows)) && crate::preview::platform_kind(ext).is_some()).then_some(true)
}

fn decode(job: &Job, bell: &Arc<Bell>) -> Option<Rgba> {
    #[cfg(test)]
    if let Some(r) = hook::fake(job) {
        return r;
    }
    let fit = [job.fit, job.fit];
    if job.os {
        return match crate::thumb::thumbnail(&job.path, fit, &job.stop, bell) {
            Some(crate::thumb::Thumb::Image(px)) => Some(px),
            _ => None,
        };
    }
    crate::preview::image_thumbnail(&job.path, fit, &job.stop)
}

/// Every grid gets its own number.
static GRID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl State {
    /// Whether pane `ix` shows a grid with cells in it: the icon view, something to show, and
    /// not covered by the preview or Get Info (which take the other pane's place when split).
    pub(crate) fn grid_on_screen(&self, ix: usize) -> bool {
        let f = &self.panes[ix].f;
        let covered = self.split() && ix != self.active && (self.preview.is_some() || self.info.is_some());
        f.view == crate::grid::ViewMode::Icons && !f.visible.is_empty() && !covered
    }

    /// The icon's side in device pixels: what thumbnails decode to.
    pub(crate) fn tile_fit(&self) -> u32 {
        (self.icon_size.icon() * self.ui_scale.get().max(1.0)).round() as u32
    }

    /// Keeps thumbnails to grids on screen: a pane whose grid isn't (another tab shown, the
    /// list view, a filter that leaves nothing, the preview over it, the folder changing)
    /// drops every thumbnail and stops what's queued or decoding for it. A grid on screen whose
    /// listing or icon size changed since its thumbnails were asked for is asked again, for
    /// the rows last seen: a reload or a new size can leave every row's key as it was, so Rune
    /// says nothing. Called after every key, command, click and listing (`sync_preview`) and
    /// from `poll`, never left to a grid's own callbacks. True when anything changed.
    pub(crate) fn sync_tiles(&mut self) -> bool {
        let mut changed = false;
        let fit = self.tile_fit();
        for ix in 0..self.panes.len() {
            if !self.grid_on_screen(ix) {
                let f = &mut self.panes[ix].f;
                changed |= f.tiles.take().is_some();
                f.grid_rows = None;
                f.tiles_for = None;
                continue;
            }
            let f = &self.panes[ix].f;
            if f.tiles_for != Some((f.version, fit))
                && let Some(rows) = f.grid_rows.clone()
            {
                let id = f.id;
                changed |= self.tiles_visible(id, rows);
            }
        }
        changed
    }

    /// Rune says rows `rows` of pane `id`'s grid are on screen: thumbnails for their cells are
    /// asked for (those not already shown or asked), and everything for cells that left is
    /// dropped or cancelled. True when the view changed (a thumbnail went).
    pub(crate) fn tiles_visible(&mut self, id: u64, rows: Range<usize>) -> bool {
        let Some(ix) = self.pane_ix(id) else { return false };
        let fit = self.tile_fit().max(1);
        if !self.grid_on_screen(ix) {
            return self.panes[ix].f.tiles.take().is_some();
        }
        let f = &mut self.panes[ix].f;
        f.grid_rows = Some(rows.clone());
        f.tiles_for = Some((f.version, fit));
        let cols = f.grid_cols.get().max(1);
        let n = f.visible.len();
        let items = (rows.start * cols).min(n)..(rows.end.saturating_mul(cols)).min(n);
        let mut changed = false;
        if f.tiles.as_ref().is_some_and(|t| t.version != f.version || t.fit != fit) {
            f.tiles = None;
            changed = true;
        }
        let here: HashSet<&OsString> = items.clone().map(|i| &f.entries[f.visible[i] as usize].os).collect();
        let mut want = Vec::new();
        for i in items.clone() {
            let e = &f.entries[f.visible[i] as usize];
            let Some(os) = wants(e) else { continue };
            let known = f.tiles.as_ref().is_some_and(|t| t.shown.contains_key(&e.os) || t.asked.contains_key(&e.os));
            if !known {
                want.push((e.os.clone(), f.cwd.join(&e.os), os));
            }
        }
        if f.tiles.is_none() && want.is_empty() {
            // Nothing to thumbnail here (no images): no pool, no work.
            return changed;
        }
        let pool = self.tile_pool.get_or_insert_with(|| Pool::start(self.waker.clone()));
        let t = f.tiles.get_or_insert_with(|| Tiles {
            shown: HashMap::new(),
            asked: HashMap::new(),
            items: 0..0,
            version: f.version,
            fit,
            id: GRID.fetch_add(1, Relaxed),
            pool: pool.shared.clone(),
        });
        t.items = items;
        let before = t.shown.len();
        t.shown.retain(|k, _| here.contains(k));
        changed |= t.shown.len() != before;
        let mut cancelled = false;
        t.asked.retain(|k, a| {
            let keep = here.contains(k);
            if !keep && let Asked::Waiting(stop) = a {
                stop.store(true, Relaxed);
                cancelled = true;
            }
            keep
        });
        if cancelled {
            t.pool.cancelled();
        }
        let grid = t.id;
        let jobs = want
            .into_iter()
            .map(|(name, path, os)| {
                let stop = Arc::new(AtomicBool::new(false));
                t.asked.insert(name.clone(), Asked::Waiting(stop.clone()));
                Job { grid, name, path, os, fit, stop }
            })
            .collect();
        pool.submit(jobs);
        changed
    }

    /// Thumbnails the workers finished, from `poll`: each lands in its grid if that grid still
    /// waits for it. True when one did.
    pub(crate) fn poll_tiles(&mut self) -> bool {
        let Some(pool) = &self.tile_pool else { return false };
        let mut changed = false;
        for d in pool.take_done() {
            let Some(t) = self.panes.iter_mut().filter_map(|p| p.f.tiles.as_mut()).find(|t| t.id == d.grid) else { continue };
            if !matches!(t.asked.get(&d.name), Some(Asked::Waiting(_))) {
                continue;
            }
            match d.rgba {
                Some((w, h, px)) => {
                    t.asked.remove(&d.name);
                    let pixels = Pixels::new();
                    pixels.set(w, h, px);
                    t.shown.insert(d.name, (pixels, [w, h]));
                    changed = true;
                }
                None => {
                    t.asked.insert(d.name, Asked::Nothing);
                }
            }
        }
        changed
    }
}

/// Test-only: a stand-in decoder that waits at a gate per path and gives a tiny image.
#[cfg(test)]
pub(crate) mod hook {
    use super::*;
    use std::path::Path;
    use std::sync::mpsc::Receiver;

    /// Paths whose decode waits until told (or until its stop flag is set).
    pub(crate) static GATES: Mutex<Vec<(PathBuf, Receiver<()>)>> = Mutex::new(Vec::new());
    /// Folders whose files decode through this stand-in (others decode for real).
    pub(crate) static FAKE_IN: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    /// Every path decoded, in order.
    pub(crate) static SEEN: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    pub(super) fn fake(job: &Job) -> Option<Option<Rgba>> {
        let dir = job.path.parent().unwrap_or(Path::new("")).to_path_buf();
        if !FAKE_IN.lock().unwrap().contains(&dir) {
            return None;
        }
        SEEN.lock().unwrap().push(job.path.clone());
        let gate = {
            let mut g = GATES.lock().unwrap();
            g.iter().position(|(p, _)| *p == job.path).map(|i| g.remove(i).1)
        };
        if let Some(go) = gate {
            while !job.stop.load(Relaxed) {
                if go.recv_timeout(std::time::Duration::from_millis(5)).is_ok() {
                    break;
                }
            }
        }
        Some(if job.stop.load(Relaxed) { None } else { Some((2, 2, vec![255; 16])) })
    }
}
