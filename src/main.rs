//! Arbor: a small, fast file manager.
//!
//! Fast by being simple: one folder read per navigation (on a worker, so a slow disk never
//! blocks the window), only the visible rows are built, and nothing animates or caches.

mod actions;
mod app;
mod commands;
mod config;
mod folder;
mod fs;
mod jump;
mod noclobber;
mod ops;
mod palette;
mod panes;
mod pattern;
mod preview;
mod select;
mod timing;
mod trash;
mod view;

use rune::{App, AppSpec, Chrome};

use app::State;

fn main() {
    // ARBOR_TIMING / ARBOR_BENCH, read once. Off, Rune gets the plain callbacks and no frame hook.
    let timed = timing::init();
    // Both overlap the platform's app launch (about 80 ms on macOS before the window is made):
    // the GPU device and shaders, and the first folder's read.
    rune::prewarm_gpu();
    app::read_first_folder();
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
    let mut app = App::new(spec).chrome(chrome);
    if timing::benching() {
        app = app.open_inactive(); // the bench never takes focus from what the user is doing
    }
    if timed { app.on_frame_encoded(timing::frame).run() } else { app.run() }
}
