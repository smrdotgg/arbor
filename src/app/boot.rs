//! The startup read: started by `main` before Rune's loop, it reads the settings, the
//! sidebar's favorites and the first folder on a thread of its own, so they overlap the
//! platform's app launch. The first window waits at most [`FIRST_PAINT_WAIT`] for them.

use super::*;

/// How long the first window waits for the settings and first folder before opening anyway:
/// one frame at 120 Hz. A local folder is read long before this matters (the read starts at the
/// top of `main`, and the platform's launch takes 60–80 ms before `State::new` on macOS), so it
/// still makes the first frame; a slow or network folder costs the window at most this much,
/// then streams in like any navigation.
pub const FIRST_PAINT_WAIT: std::time::Duration = std::time::Duration::from_millis(8);

/// Headless runs (scripts, snapshots) run on a virtual clock and must be deterministic, so
/// they wait for the first listing (bounded, in case the folder never answers).
pub(super) const HEADLESS_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// The settings and the folder to open, from the startup read.
pub(super) struct Start {
    pub(super) config: Config,
    pub(super) start: PathBuf,
    pub(super) places: Vec<(String, PathBuf)>,
    /// A problem with the start folder (or the sidebar's file).
    pub(super) note: Option<String>,
    /// `places` are the user's own list, from `sidebar.toml`.
    pub(super) custom: bool,
}

/// What the startup read sends, in this order.
pub(super) enum Boot {
    Start(Box<Start>),
    /// The first folder, sorted by this.
    Listing(Sort, Listing),
}

/// The startup read: a thread reading the settings, then the first folder, sending each as it
/// finishes and waking the window once it has one.
pub(super) struct Booting {
    pub(super) rx: std::sync::mpsc::Receiver<Boot>,
    pub(super) waker: std::sync::Arc<std::sync::Mutex<Option<rune::Waker>>>,
}

impl Booting {
    pub(super) fn start() -> Booting {
        let slow = std::time::Duration::from_millis(SLOW_FIRST_READ_MS.load(std::sync::atomic::Ordering::Relaxed));
        Booting::spawn(slow, || {
            let config = Config::load();
            let (start, note) = start_dir(&config.settings);
            // The user's favorites (sidebar.toml), or the defaults.
            let fav = crate::sidebar::load(crate::sidebar::file_path().as_deref());
            let custom = fav.list.is_some();
            let places = fav.list.map_or_else(|| places_for(&config.settings), crate::sidebar::named);
            let note = match (note, fav.error) {
                (Some(a), Some(b)) => Some(format!("{a} · {b}")),
                (a, b) => a.or(b),
            };
            Start { config, start, places, note, custom }
        })
    }

    /// Runs `settings` then lists the folder it names (`slow` later), on a new thread.
    pub(super) fn spawn(slow: std::time::Duration, settings: impl FnOnce() -> Start + Send + 'static) -> Booting {
        let (tx, rx) = std::sync::mpsc::channel();
        let waker: std::sync::Arc<std::sync::Mutex<Option<rune::Waker>>> = Default::default();
        let wake = waker.clone();
        let send = move |b: Boot| {
            // The window may be gone (or not yet open, when `setup` hasn't run: then `poll` on
            // its first frame picks this up).
            if tx.send(b).is_ok()
                && let Some(w) = wake.lock().ok().and_then(|w| w.clone())
            {
                w();
            }
        };
        std::thread::spawn(move || {
            let start = settings();
            let (dir, sort) = (start.start.clone(), start.config.settings.sort);
            send(Boot::Start(Box::new(start)));
            if !slow.is_zero() {
                std::thread::sleep(slow);
            }
            send(Boot::Listing(sort, fs::list(dir, sort)));
        });
        Booting { rx, waker }
    }
}

/// `ARBOR_SLOW_FIRST_READ_MS`, read by `timing::init` only when timing is on (otherwise 0 and
/// never set): makes the first folder read that much slower, to check that a slow disk doesn't
/// hold up the window.
pub static SLOW_FIRST_READ_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The sidebar: the settings file's places, or the ones found on this machine.
pub(super) fn places_for(s: &Settings) -> Vec<(String, PathBuf)> {
    let Some(list) = &s.places else { return fs::places() };
    crate::sidebar::named(list.clone())
}

/// The startup read, started by `main` before Rune's event loop so it overlaps the platform's
/// app launch instead of following it.
pub(super) static BOOT: std::sync::Mutex<Option<Booting>> = std::sync::Mutex::new(None);

/// Starts reading the settings file and the folder Arbor opens in. Call first thing in `main`.
pub fn read_first_folder() {
    let boot = Booting::start();
    if let Ok(mut slot) = BOOT.lock() {
        *slot = Some(boot);
    }
}

/// The folder named on the command line, else the settings' start folder, else the current
/// one (or home when launched from `/`). Also a note when the start folder isn't there.
pub(super) fn start_dir(s: &Settings) -> (PathBuf, Option<String>) {
    if let Some(p) = std::env::args_os().skip(1).map(PathBuf::from).find(|p| p.is_dir()) {
        return (p, None);
    }
    let mut note = None;
    if let Some(p) = &s.start_folder {
        if p.is_dir() {
            return (p.clone(), None);
        }
        note = Some(format!("Settings: start_folder {} isn't a folder", fs::pretty(p)));
    }
    let dir = std::env::current_dir().ok().filter(|p| p != Path::new("/")).unwrap_or_else(fs::home);
    (dir, note)
}
