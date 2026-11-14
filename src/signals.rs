//! Unix: SIGTERM, SIGINT and SIGHUP quit Satchel the way ⌘Q does (a logout, `kill`, Ctrl+C in a
//! terminal, the terminal closing), so a running operation is cancelled and what it left is
//! recorded (`leftovers.rs`), and the user-data writers get their moment.
//!
//! The handler does only what's async-signal-safe: it writes the signal's number to a pipe
//! (non-blocking, so it can't block; errno kept). A thread of its own (`satchel-signals`) sleeps
//! reading that pipe (no polling) and runs the quit on the UI thread's terms
//! (`window::quit_app`: Rune's quit, every window's quit hook, then `main`'s `last`). After the
//! first signal the three go back to their default action, so a second one ends the process at
//! once if the quit doesn't. A child process gets the default handlers back when it `exec`s, and
//! the pipe is close-on-exec.
//!
//! Windows has no such signals for a windowed app: a logoff or shutdown reaches Rune as
//! `WM_QUERYENDSESSION`/`WM_ENDSESSION`, which run every window's quit hook the same way.

use std::sync::atomic::{AtomicI32, Ordering};

/// The signals forwarded.
const SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// The pipe's write end, for the handler (-1 until `forward` made it).
static WRITE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(sig: libc::c_int) {
    let fd = WRITE.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    // SAFETY: `errno` is this thread's; reading and restoring it, and `write` to a pipe, are
    // async-signal-safe. A full pipe drops the byte (non-blocking): a quit is already on its way.
    unsafe {
        let errno = errno_location();
        let saved = *errno;
        let byte = sig as u8;
        let _ = libc::write(fd, (&raw const byte).cast(), 1);
        *errno = saved;
    }
}

#[cfg(target_os = "linux")]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: always valid for the calling thread.
    unsafe { libc::__errno_location() }
}

#[cfg(target_os = "macos")]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: always valid for the calling thread.
    unsafe { libc::__error() }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: always valid for the calling thread.
    unsafe { libc::__errno() }
}

/// From now on the three signals call `quit` (with the signal's number) on the `satchel-signals`
/// thread, the first one only. Once per process; a failure leaves the default actions.
pub fn forward(quit: fn(libc::c_int)) {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for the two descriptors.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        eprintln!("satchel: no pipe for termination signals: {}", std::io::Error::last_os_error());
        return;
    }
    let [read, write] = fds;
    for fd in fds {
        // SAFETY: descriptors this call just made.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    // SAFETY: the write end, made above.
    unsafe { libc::fcntl(write, libc::F_SETFL, libc::fcntl(write, libc::F_GETFL) | libc::O_NONBLOCK) };
    if WRITE.compare_exchange(-1, write, Ordering::AcqRel, Ordering::Relaxed).is_err() {
        // SAFETY: ours, unused.
        unsafe {
            libc::close(read);
            libc::close(write);
        }
        return;
    }
    let spawned = std::thread::Builder::new().name("satchel-signals".into()).spawn(move || {
        let mut byte = 0u8;
        loop {
            // SAFETY: one byte into `byte`, from the read end this thread owns.
            let n = unsafe { libc::read(read, (&raw mut byte).cast(), 1) };
            if n == 1 {
                break;
            }
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        // A second signal ends the process at once.
        for sig in SIGNALS {
            // SAFETY: restoring the default action.
            unsafe { libc::signal(sig, libc::SIG_DFL) };
        }
        quit(libc::c_int::from(byte));
    });
    if let Err(e) = spawned {
        eprintln!("satchel: no thread for termination signals: {e}");
        return;
    }
    for sig in SIGNALS {
        // SAFETY: a handler that only writes to a pipe (above); SA_RESTART so a system call the
        // signal interrupts on another thread carries on.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(sig, &action, std::ptr::null_mut());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::mpsc::{Sender, channel};
    use std::time::Duration;

    static TOLD: Mutex<Option<Sender<libc::c_int>>> = Mutex::new(None);

    fn told(sig: libc::c_int) {
        if let Some(tx) = TOLD.lock().unwrap().as_ref() {
            let _ = tx.send(sig);
        }
    }

    /// A SIGTERM reaches the quit on the forwarding thread (not in the handler), once, and
    /// leaves the default action behind it. (SIGTERM is raised once in the whole test binary:
    /// a second would end it.)
    #[test]
    fn a_termination_signal_runs_the_quit_on_its_thread() {
        let (tx, rx) = channel();
        *TOLD.lock().unwrap() = Some(tx);
        super::forward(told);
        // SAFETY: our own process; the handler above catches it.
        unsafe { libc::raise(libc::SIGTERM) };
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(libc::SIGTERM));
        let mut now: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: reading the current action into `now`.
        unsafe { libc::sigaction(libc::SIGTERM, std::ptr::null(), &mut now) };
        assert_eq!(now.sa_sigaction, libc::SIG_DFL, "a second SIGTERM would end the process");
    }
}
