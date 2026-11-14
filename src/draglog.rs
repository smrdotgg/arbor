//! `SATCHEL_DRAG_LOG=1`: what happens around a row's drag, one line per event on stderr, for a
//! run on a real Mac (where a drag can't be driven from a test):
//!
//! ```text
//! satchel-drag t=1234.5 drag-out ask row="a.txt" ev=LeftMouseDragged pressed=1 mode=kCFRunLoopDefaultMode -> offered
//! satchel-drag t=1250.9 drag-out row="a.txt"
//! satchel-drag t=1250.9 drag-out started pressed=0 -> released (posted a left-mouse-up)
//! satchel-drag t=1251.3 drag-out end row="a.txt" outcome=Cancelled ev=LeftMouseUp pressed=0 mode=kCFRunLoopDefaultMode
//! satchel-drag t=1803.0 loop mode=kCFRunLoopDefaultMode
//! satchel-drag t=2101.4 appkit KeyDown chars="j" mode=kCFRunLoopDefaultMode
//! satchel-drag t=2101.6 key Command('j') cmd -> Jump
//! ```
//!
//! - `drag-out ask`: Rune asked a row what to drag (the pointer crossed the threshold), with
//!   the AppKit event being handled, the mouse buttons the window server says are down
//!   (`pressed`, a bit mask; synthetic input from automation tools may not set it) and the run
//!   loop's mode. `drag-out started` (when Rune says the system's session began, a frame
//!   later, with the buttons down then, and whether Satchel gave a session left behind by its
//!   release that release again) and `drag-out end` bracket the session.
//! - `loop`: the main run loop's mode, each time it changes, seen from `poll` (every frame's).
//!   `NSEventTrackingRunLoopMode` is a tracking loop running (a drag session, a menu).
//! - `appkit`: every key press and left-button press, drag and release AppKit dispatches to the
//!   app (a local event monitor). A nested tracking loop takes its events without dispatching
//!   them, so a key typed while a drag session is still running shows no `appkit` line.
//! - `key`: a key that reached Satchel, and the command it ran (or `unhandled`).
//! - `drop-hover` and `drop`: a drag over a folder row or the list, and a drop on one.
//! - `note`: the status bar told about a drag nothing took, for a drag-out that was refused.
//!
//! Off, nothing here runs but one check of a flag read once.

use std::sync::OnceLock;
use std::time::Instant;

static ON: OnceLock<Option<Instant>> = OnceLock::new();

/// Whether `SATCHEL_DRAG_LOG` is on (read once).
pub fn on() -> bool {
    ON.get_or_init(|| std::env::var_os("SATCHEL_DRAG_LOG").is_some_and(|v| !v.is_empty() && v != "0").then(Instant::now)).is_some()
}

/// Logs a line (built only when on).
pub fn log(line: impl FnOnce() -> String) {
    if let Some(Some(start)) = ON.get() {
        eprintln!("satchel-drag t={:.1} {}", start.elapsed().as_secs_f64() * 1000.0, line());
    }
}

/// The run loop mode last logged, so `loop` lines show only changes.
static MODE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// From `poll`: the run loop's mode, when it changed.
pub fn poll() {
    if !on() {
        return;
    }
    let mode = run_loop_mode();
    let mut last = MODE.lock().unwrap_or_else(|e| e.into_inner());
    if *last != mode {
        log(|| format!("loop mode={mode}"));
        *last = mode;
    }
}

/// The AppKit event being handled, the mouse buttons down and the run loop's mode.
pub fn appkit() -> String {
    format!("{} mode={}", event_and_buttons(), run_loop_mode())
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    /// An `NSEventType`'s name, for the kinds the log cares about.
    pub(super) fn kind_name(kind: usize) -> String {
        match kind {
            1 => "LeftMouseDown".into(),
            2 => "LeftMouseUp".into(),
            5 => "MouseMoved".into(),
            6 => "LeftMouseDragged".into(),
            10 => "KeyDown".into(),
            11 => "KeyUp".into(),
            12 => "FlagsChanged".into(),
            n => format!("type{n}"),
        }
    }

    /// `s` as a Rust string (an `NSString`, or nil).
    ///
    /// SAFETY: `s` is nil or a live `NSString`.
    pub(super) unsafe fn string(s: *mut AnyObject) -> String {
        if s.is_null() {
            return "nil".into();
        }
        // SAFETY: as the caller says; UTF8String lives as long as the string's autorelease pool.
        unsafe {
            let p: *const std::ffi::c_char = msg_send![s, UTF8String];
            if p.is_null() { "?".into() } else { std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned() }
        }
    }

    pub(super) fn main_thread() -> bool {
        // SAFETY: a class method with no arguments.
        unsafe { msg_send![class!(NSThread), isMainThread] }
    }

    pub(super) fn event_and_buttons() -> String {
        if !main_thread() {
            return "ev=? pressed=?".into();
        }
        // SAFETY: AppKit getters on the main thread.
        unsafe {
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            let ev: *mut AnyObject = if app.is_null() { std::ptr::null_mut() } else { msg_send![app, currentEvent] };
            let kind = if ev.is_null() {
                "none".into()
            } else {
                let k: usize = msg_send![ev, type];
                kind_name(k)
            };
            let buttons: usize = msg_send![class!(NSEvent), pressedMouseButtons];
            format!("ev={kind} pressed={buttons}")
        }
    }

    pub(super) fn run_loop_mode() -> String {
        if !main_thread() {
            return "?".into();
        }
        // SAFETY: the main thread's run loop and its mode, read on the main thread.
        unsafe {
            let rl: *mut AnyObject = msg_send![class!(NSRunLoop), currentRunLoop];
            let mode: *mut AnyObject = msg_send![rl, currentMode];
            string(mode)
        }
    }

    /// Logs every key press and left-button press, drag and release AppKit dispatches.
    pub(super) fn monitor() {
        use block2::RcBlock;
        // NSEventMaskLeftMouseDown | LeftMouseUp | LeftMouseDragged | KeyDown.
        let mask: u64 = (1 << 1) | (1 << 2) | (1 << 6) | (1 << 10);
        let block = RcBlock::new(|ev: *mut AnyObject| -> *mut AnyObject {
            if !ev.is_null() {
                // SAFETY: AppKit hands the monitor a live event, on the main thread.
                let (kind, chars) = unsafe {
                    let k: usize = msg_send![ev, type];
                    let chars = if k == 10 {
                        let c: *mut AnyObject = msg_send![ev, charactersIgnoringModifiers];
                        format!(" chars={:?}", string(c))
                    } else {
                        String::new()
                    };
                    (k, chars)
                };
                super::log(|| format!("appkit {}{chars} mode={}", kind_name(kind), run_loop_mode()));
            }
            ev
        });
        // SAFETY: a documented class method; the monitor (and the block it copies) stays for
        // the life of the app, as logging does.
        unsafe {
            let token: *mut AnyObject = msg_send![class!(NSEvent), addLocalMonitorForEventsMatchingMask: mask, handler: &*block];
            if !token.is_null() {
                let _: *mut AnyObject = msg_send![token, retain];
            }
        }
    }
}

#[cfg(target_os = "macos")]
use mac::{event_and_buttons, run_loop_mode};

#[cfg(not(target_os = "macos"))]
fn event_and_buttons() -> String {
    "ev=- pressed=-".into()
}

#[cfg(not(target_os = "macos"))]
fn run_loop_mode() -> String {
    "-".into()
}

/// Starts the AppKit event monitor (macOS), once the app runs. Nothing when off.
pub fn install() {
    if !on() {
        return;
    }
    log(|| "logging drags, drops and keys".into());
    #[cfg(target_os = "macos")]
    if mac::main_thread() {
        mac::monitor();
    }
}

#[cfg(test)]
mod tests {
    /// Off (tests never set `SATCHEL_DRAG_LOG`), a line isn't even built.
    #[test]
    fn off_builds_nothing() {
        assert!(!super::on());
        super::log(|| panic!("built while off"));
        super::poll();
        super::install();
    }
}
