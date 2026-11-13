//! Getting the window's GPU ready early: started first thing in `main` (not for headless runs,
//! which make no window).
//!
//! - **macOS**: `rune::prewarm_gpu` makes the adapter, device and uber pipeline on a thread while
//!   AppKit starts; the first window takes them.
//! - **Linux**: `rune::warm_gpu` makes the window's device and renderer on a thread while the
//!   event loop, fonts and window are made, and the window's surface takes them (under Xvfb
//!   127 → 101 ms to the first frame, BENCH.md 2026-11-11).
//! - **Windows**: nothing. Rune draws a window's first frame on the CPU and shows it before any
//!   GPU exists, then makes the GPU on a below-normal thread after that frame and hands over
//!   with no visible change (Rune #689). Starting the GPU here would compete with that first
//!   frame for the CPU and the loader lock (a D3D12 driver's DLLs), which is what Rune's order
//!   avoids. Rune also picks the adapter from DXGI's list without making a device on the others
//!   (#625, through the wgpu-hal patch in Cargo.toml), so the adapter list Arbor used to hold
//!   here would now only wake every GPU for nothing (DECISIONS.md, 2026-11-13).

/// Starts getting the GPU ready, where that helps (above).
pub fn start() {
    // Does nothing off macOS.
    rune::prewarm_gpu();
    #[cfg(all(unix, not(target_os = "macos")))]
    rune::warm_gpu(wgpu::Features::empty());
}
