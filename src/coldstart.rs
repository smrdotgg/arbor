//! Cold-start probes' clock: how long since the OS created this process, and the line each
//! probe appends to `SATCHEL_TIMING_FILE`. Shared by Satchel (`timing.rs`) and the bare probes in
//! `examples/` (`win32_hello`, `rune_hello`), which include this file by path, so it uses
//! nothing else from the crate.
//!
//! The line is `<name> <ms> from=<process|main> epoch_us=<µs>`: `ms` is from the process's
//! creation (Windows: `GetProcessTimes`, which counts the loader, DLL imports and everything
//! before `main`) to the first frame, or from `main` where the OS doesn't say when the process
//! was created to the microsecond (Linux and macOS: a launcher subtracts its own spawn time from
//! `epoch_us`, the first frame on the wall clock, as `scripts/bench.sh` does).

// Each of the three binaries including this uses a different part of it.
#![allow(dead_code)]

use std::io::Write;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Milliseconds from the process's creation to now, where the OS keeps it precisely.
#[cfg(windows)]
pub fn since_process_created() -> Option<f64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the pseudo-handle needs no closing, and the four out-pointers are live locals.
    let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) };
    if ok == 0 {
        return None;
    }
    // FILETIME counts 100 ns since 1601; SystemTime is GetSystemTimePreciseAsFileTime's, from 1970.
    let created = ((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime)) as i128 - 116_444_736_000_000_000;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_nanos() as i128 / 100;
    Some((now - created) as f64 / 10_000.0)
}

#[cfg(not(windows))]
pub fn since_process_created() -> Option<f64> {
    None
}

/// Microseconds since 1970 now.
pub fn epoch_us() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros())
}

/// The probe's line for a first frame now: from the process's creation, else from `main`.
pub fn line(name: &str, main: Instant) -> String {
    let (ms, from) = match since_process_created() {
        Some(ms) => (ms, "process"),
        None => (main.elapsed().as_secs_f64() * 1000.0, "main"),
    };
    format!("{name} {ms:.2} from={from} epoch_us={}", epoch_us())
}

/// Appends `line` to `path`. A probe has no console to report a failure on, so it's dropped.
pub fn append(path: &std::ffi::OsStr, line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}
