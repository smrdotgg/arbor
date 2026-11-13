//! Arbor: a small, fast file manager.
//!
//! Fast by being simple: one folder read per navigation (on a worker, so a slow disk never
//! blocks the window), only the visible rows are built, and nothing animates or caches.

// Windows: a window, not a console, when started from Explorer (release builds; debug builds
// keep the console for logs).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(test)]
mod a11y_tests;
mod actions;
mod app;
mod archive;
mod band;
mod batch;
mod bounded;
mod coldstart;
mod columns;
mod commands;
mod complete;
mod config;
mod confirm;
mod delete;
#[cfg(unix)]
mod dirfd;
mod draglog;
mod ffi;
mod folder;
#[cfg(test)]
mod frame_bench;
mod fs;
mod gpu;
mod grid;
mod handler;
mod incoming;
mod info;
mod jump;
mod leftovers;
mod listcols;
mod marked;
mod menu;
mod mounts;
mod noclobber;
mod openwith;
mod ops;
#[cfg(all(test, unix, not(target_os = "macos")))]
mod packaging_tests;
mod palette;
mod panes;
mod pattern;
mod persist;
mod preview;
#[cfg(test)]
mod prop;
mod rename;
mod search;
mod select;
mod sidebar;
mod thumb;
mod tiles;
mod timing;
mod trash;
mod view;
mod volinfo;
mod volumes;
mod window;

use rune::App;

fn main() {
    // `arbor --version`: what's installed, without a window (CI runs the Mac package's Intel
    // slice this way, under Rosetta).
    if std::env::args_os().nth(1).is_some_and(|a| a == "--version") {
        println!("arbor {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    // For uninstallers: put back the folder default Arbor replaced, without opening a window.
    if std::env::args_os().nth(1).is_some_and(|a| a == "--restore-folder-default") {
        match handler::run(handler::Change::Restore) {
            Ok(m) => println!("{m}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        return;
    }
    // ARBOR_TIMING / ARBOR_BENCH, read once. Off, Rune gets the plain callbacks and no frame hook.
    let timed = timing::init();
    // Whether Rune will render offscreen (snapshots, scripts, traces) instead of opening a
    // window, from the arguments, before `run`: `headless_run()` only answers once `run` has
    // started one, too late for what follows.
    let windowed = !rune::is_headless();
    // Windows: `arbor.exe <folder>` with an Arbor already running hands it the folder (a new
    // tab there) and exits before any window.
    #[cfg(windows)]
    if !timing::benching() && windowed && incoming::windows::hand_off_args() {
        return;
    }
    // Both overlap the platform's app launch (about 80 ms on macOS before the window is made):
    // the GPU device and shaders, and the first folder's read.
    // Windows and Linux (where `prewarm_gpu` does nothing): the window's GPU device and renderer
    // made on a thread while Rune builds its event loop and window (`gpu.rs`). Headless runs
    // make no window, so neither.
    if windowed {
        rune::prewarm_gpu();
        gpu::start();
    }
    app::read_first_folder();
    // Linux: the window's app id (Wayland) and WM_CLASS (X11), matching arbor.desktop.
    rune::set_app_id("arbor");
    // Every window has the same callbacks (`window.rs`); the first is timed when timing is on.
    let mut spec = window::spec();
    if timed {
        spec.init = timing::init_state;
        spec.setup = Some(timing::setup);
        spec.poll = Some(timing::poll);
        spec.on_key = Some(timing::on_key);
    }
    let mut app = window::configure(App::new(spec), window::FIRST);
    window::register(window::FIRST, app.handle());
    if timed {
        timing::hold(app.handle());
    }
    if timing::benching() {
        app = app.open_inactive(); // the bench never takes focus from what the user is doing
    } else if windowed {
        // Folders other apps ask for while Arbor runs (or that launched it): the window
        // focused last opens them.
        incoming::listen();
        // Volumes coming and going, where the OS says so without polling: every window.
        volumes::watch();
    }
    if timed {
        app.on_frame_encoded(timing::frame).run()
    } else {
        app.run()
    }
    // Whatever `gpu::start` still holds (Rune made no device, or no window ever showed) goes.
    gpu::release();
    // No window is on screen now: a file operation the last window's close cut short gets its
    // bounded cleanup, and what it left is recorded (`leftovers.rs`).
    leftovers::finish();
    persist::drain_at_exit();
}
