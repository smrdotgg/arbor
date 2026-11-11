//! Cold-start probe: the smallest Rune app (one text view), built with Arbor's Rune features
//! and profile, to tell Rune's share of a launch from Arbor's. Started like Arbor (the GPU
//! prewarm first thing in `main`), it appends `rune-hello <ms> …` to `ARBOR_TIMING_FILE` at
//! its first drawn frame and quits (see `src/coldstart.rs` for what `ms` counts).
//!
//! ```text
//! cargo build --profile dist --example rune_hello
//! ```

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[path = "../src/coldstart.rs"]
mod coldstart;

use std::time::Instant;

use rune::{App, AppSpec, Cx, El, FrameOutcome, text};

fn view(_: &(), _: &mut Cx<()>) -> El {
    text("hello")
}

fn main() {
    let main = Instant::now();
    rune::prewarm_gpu();
    let app = App::new(AppSpec::new("rune-hello", (980.0, 620.0), || (), view));
    let handle = app.handle();
    let file = std::env::var_os("ARBOR_TIMING_FILE");
    let mut done = false;
    app.on_frame_encoded(move |f| {
        if done || f.outcome() != FrameOutcome::Drawn {
            return;
        }
        done = true;
        let line = coldstart::line("rune-hello", main);
        match &file {
            Some(path) => coldstart::append(path, &line),
            None => eprintln!("{line}"),
        }
        handle.quit();
    })
    .run();
}
