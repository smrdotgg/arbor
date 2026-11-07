//! Arbor: a small, fast file manager.
//!
//! Fast by being simple: one folder read per navigation (on a worker, so a slow disk never
//! blocks the window), only the visible rows are built, and nothing animates or caches.

mod app;
mod fs;
mod view;

use rune::{App, AppSpec, Chrome};

use app::State;

fn main() {
    let spec = AppSpec::<State> {
        min_size: Some((520.0, 320.0)),
        setup: Some(|s, waker| s.setup(waker)),
        poll: Some(|s, _, tx| s.poll(tx)),
        on_key: Some(|s, ev, tx| s.on_key(ev, tx)),
        ..AppSpec::new("Arbor", (980.0, 620.0), State::new, view::view)
    };
    // macOS: content runs under a transparent titlebar; the traffic lights sit in the sidebar's
    // top band and the path bar is the drag region. Other platforms keep their native frame.
    let chrome = Chrome { full_size_content: true, traffic_lights: Some((14.0, view::BAR_H / 2.0 - 7.0)), drag_regions: true };
    App::new(spec).chrome(chrome).run();
}
