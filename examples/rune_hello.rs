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

use std::sync::OnceLock;
use std::time::Instant;

use rune::{App, AppSpec, Cx, El, FrameOutcome, text};

static GPU_STARTING: OnceLock<f64> = OnceLock::new();
static GPU_DREW: OnceLock<f64> = OnceLock::new();
static WINDOW: OnceLock<f64> = OnceLock::new();

fn view(_: &(), _: &mut Cx<()>) -> El {
    text("hello")
}

fn main() {
    let main = Instant::now();
    // When Rune starts making the GPU device, and when it first draws (`rune::on_gpu`), and
    // when the native window exists (`App::on_window`), from the process's creation (from
    // `main` where the OS doesn't say).
    let now = move || coldstart::since_process_created().unwrap_or_else(|| main.elapsed().as_secs_f64() * 1000.0);
    rune::on_gpu(move |event| {
        let slot = match event {
            rune::GpuEvent::Starting => &GPU_STARTING,
            rune::GpuEvent::Drew => &GPU_DREW,
        };
        let _ = slot.set(now());
    });
    rune::prewarm_gpu();
    let app = App::new(AppSpec::new("rune-hello", (980.0, 620.0), || (), view)).on_window(move |_| {
        let _ = WINDOW.set(now());
    });
    let handle = app.handle();
    let file = std::env::var_os("ARBOR_TIMING_FILE");
    let mut done = false;
    app.on_frame_encoded(move |f| {
        if done || f.outcome() != FrameOutcome::Drawn {
            return;
        }
        done = true;
        let at = |slot: &OnceLock<f64>| slot.get().copied().unwrap_or(f64::NAN);
        let line = format!(
            "{} gpu_starting_ms={:.2} window_ms={:.2} gpu_drew_ms={:.2}",
            coldstart::line("rune-hello", main),
            at(&GPU_STARTING),
            at(&WINDOW),
            at(&GPU_DREW)
        );
        match &file {
            Some(path) => coldstart::append(path, &line),
            None => eprintln!("{line}"),
        }
        handle.quit();
    })
    .run();
}
