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
mod batch;
mod commands;
mod complete;
mod config;
mod confirm;
mod delete;
mod ffi;
mod folder;
#[cfg(test)]
mod frame_bench;
mod fs;
mod handler;
mod incoming;
mod info;
mod jump;
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
mod rename;
mod search;
mod select;
mod sidebar;
mod thumb;
mod timing;
mod trash;
mod view;
mod volumes;

use rune::{App, AppSpec, Chrome};

use app::State;

fn main() {
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
    // Windows: `arbor.exe <folder>` with an Arbor already running hands it the folder (a new
    // tab there) and exits before any window.
    #[cfg(windows)]
    if !timing::benching() && rune::tooling::headless_run().is_none() && incoming::windows::hand_off_args() {
        return;
    }
    // Both overlap the platform's app launch (about 80 ms on macOS before the window is made):
    // the GPU device and shaders, and the first folder's read.
    rune::prewarm_gpu();
    app::read_first_folder();
    // Linux: the window's app id (Wayland) and WM_CLASS (X11), matching arbor.desktop.
    rune::set_app_id("arbor");
    let mut spec = AppSpec::<State> {
        min_size: Some((520.0, 320.0)),
        setup: Some(|s, waker| s.setup(waker)),
        poll: Some(|s, _, tx| s.poll(tx)),
        on_key: Some(|s, ev, tx| s.on_key(ev, tx)),
        ..AppSpec::new("Arbor", (980.0, 620.0), State::new, view::view)
    };
    if timed {
        spec.init = timing::init_state;
        spec.setup = Some(timing::setup);
        spec.poll = Some(timing::poll);
        spec.on_key = Some(timing::on_key);
    }
    // macOS: content runs under a transparent titlebar; the traffic lights sit in the sidebar's
    // top band and the path bar is the drag region. Other platforms keep their native frame.
    let chrome = Chrome { full_size_content: true, traffic_lights: Some((14.0, view::BAR_H / 2.0 - 7.0)), drag_regions: true };
    // Searches stop first; then what's still queued for the user-data writers (the Jump
    // history, the sidebar) gets a moment to land.
    let mut app = App::new(spec).chrome(chrome).on_close(|s, _| {
        s.stop_background();
        persist::drain_at_exit();
    });
    if timing::benching() {
        app = app.open_inactive(); // the bench never takes focus from what the user is doing
    } else if rune::tooling::headless_run().is_none() {
        // Folders other apps ask for while Arbor runs (or that launched it).
        incoming::listen(app.handle());
        // Volumes coming and going, where the OS says so without polling.
        volumes::watch(app.handle());
    }
    if timed {
        app.on_frame_encoded(timing::frame).run()
    } else {
        app.run()
    }
    persist::drain_at_exit();
}
