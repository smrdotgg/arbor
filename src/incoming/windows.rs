use super::{Sink, classify};
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FlushFileBuffers, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::CancelSynchronousIo;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

use crate::ffi::wide;

/// The pipe for this user's login session (a fast user switch or a remote session gets its
/// own Arbor).
pub fn pipe_name() -> String {
    let mut session = 0u32;
    // SAFETY: a valid out pointer; on failure `session` stays 0.
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    let user: String =
        std::env::var("USERNAME").unwrap_or_default().chars().filter(|c| c.is_alphanumeric() || "-_.".contains(*c)).collect();
    format!(r"\\.\pipe\gg.smr.arbor.{session}.{user}")
}

/// How long a second Arbor waits, once connected, for the running one to take its paths
/// and say so; then it opens its own window instead.
pub const DEADLINE: std::time::Duration = std::time::Duration::from_secs(1);

/// What the running Arbor answers once it has read a request.
const ACK: u8 = 1;

/// The paths as the pipe carries them: UTF-16 code units, each path ended by a NUL, and one
/// more NUL to end the request (so the running Arbor knows it's all there and can answer
/// on the same pipe). An older client just closes its end.
pub fn encode(paths: &[PathBuf]) -> Vec<u8> {
    paths
        .iter()
        .filter(|p| !p.as_os_str().is_empty())
        .flat_map(|p| p.as_os_str().encode_wide().chain([0]))
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// Whether `bytes` holds a whole request (it ends with the extra NUL).
fn complete(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && bytes.len().is_multiple_of(2) && bytes[bytes.len() - 4..] == [0, 0, 0, 0]
}

pub fn decode(bytes: &[u8]) -> Vec<PathBuf> {
    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|&b| u16::from_le_bytes(b)).collect();
    units.split(|&u| u == 0).filter(|p| !p.is_empty()).map(|p| PathBuf::from(OsString::from_wide(p))).collect()
}

/// Hands `paths` to the Arbor serving `name`: true when it took them (this process then
/// exits). False when none is running (or it couldn't be reached): this one opens its own
/// window. Lets the running one bring its window to the front, which Windows allows only
/// with the say-so of the process the user just started.
/// At startup: the paths on the command line, if any, to a running Arbor. Touches no disk
/// (the running one sorts folders from files on its pipe's thread); without arguments it
/// does nothing, so a plain launch costs nothing and opens a window as before.
pub fn hand_off_args() -> bool {
    let paths: Vec<PathBuf> = std::env::args_os().skip(1).filter(|a| !a.to_string_lossy().starts_with("--")).map(PathBuf::from).collect();
    if paths.is_empty() {
        return false;
    }
    let paths: Vec<PathBuf> = paths.into_iter().map(|p| std::path::absolute(&p).unwrap_or(p)).collect();
    hand_off(&pipe_name(), &paths)
}

pub fn hand_off(name: &str, paths: &[PathBuf]) -> bool {
    let name = wide(name);
    for _ in 0..3 {
        // SAFETY: `name` is NUL-terminated; no security attributes or template.
        let h: HANDLE = unsafe {
            CreateFileW(name.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut())
        };
        if h == INVALID_HANDLE_VALUE {
            // SAFETY: plain calls; `name` is NUL-terminated.
            if unsafe { GetLastError() } == ERROR_PIPE_BUSY && unsafe { WaitNamedPipeW(name.as_ptr(), 2000) } != 0 {
                continue;
            }
            return false;
        }
        let mut pid = 0u32;
        // SAFETY: `h` is the open pipe; `pid` a valid out pointer.
        if unsafe { GetNamedPipeServerProcessId(h, &mut pid) } != 0 {
            unsafe { AllowSetForegroundWindow(pid) };
        }
        return exchange(h, encode(paths), DEADLINE);
    }
    false
}

/// Writes the request on the connected pipe `h` and waits for the running Arbor's answer,
/// on a thread of its own, for at most `deadline`: a running Arbor that's hung (or stuck on
/// an earlier request) never holds this launch up. On time out the thread's blocked call is
/// cancelled and false comes back (this one opens its own window). Closes `h`.
fn exchange(h: HANDLE, bytes: Vec<u8>, deadline: std::time::Duration) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    let pipe = h as usize;
    let worker = std::thread::spawn(move || {
        let h = pipe as HANDLE;
        let mut sent = 0usize;
        let mut ok = true;
        while sent < bytes.len() {
            let mut n = 0u32;
            let chunk = (bytes.len() - sent).min(u32::MAX as usize) as u32;
            // SAFETY: the buffer is valid for `chunk` bytes from `sent`.
            if unsafe { WriteFile(h, bytes[sent..].as_ptr(), chunk, &mut n, std::ptr::null_mut()) } == 0 || n == 0 {
                ok = false;
                break;
            }
            sent += n as usize;
        }
        let mut ack = [0u8];
        let mut n = 0u32;
        // SAFETY: `h` is the open pipe; `ack` is valid for one byte.
        ok = ok
            && unsafe { FlushFileBuffers(h) } != 0
            && unsafe { ReadFile(h, ack.as_mut_ptr(), 1, &mut n, std::ptr::null_mut()) } != 0
            && n == 1
            && ack[0] == ACK;
        // SAFETY: `h` is ours to close.
        unsafe { CloseHandle(h) };
        let _ = tx.send(ok);
    });
    match rx.recv_timeout(deadline) {
        Ok(ok) => ok,
        Err(_) => {
            use std::os::windows::io::AsRawHandle;
            // SAFETY: the worker's own thread handle, alive while `worker` is; it then
            // closes the pipe and ends.
            unsafe { CancelSynchronousIo(worker.as_raw_handle() as HANDLE) };
            false
        }
    }
}

/// Serves `name` on a thread of its own, sending what each client writes to `sink`. Does
/// nothing if another process already serves it.
pub fn serve(name: String, sink: Sink) {
    let wname = wide(&name);
    let first = make(&wname, true);
    if first == INVALID_HANDLE_VALUE {
        return;
    }
    // Handles are plain values: the thread owns this one from here.
    let first = first as usize;
    std::thread::Builder::new()
        .name("arbor-pipe".into())
        .spawn(move || {
            let mut h = first as HANDLE;
            loop {
                // SAFETY: `h` is a pipe instance this thread made; blocks until a client
                // connects (or one connected before this call: ERROR_PIPE_CONNECTED).
                let connected =
                    unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) } != 0 || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
                // The next instance first, so there's always one waiting: a client between
                // this one closing and a new one opening would find no pipe and open a
                // window of its own.
                let next = make(&wname, false);
                if connected {
                    let paths = decode(&read_all(h));
                    // Taken: the client can go (it waits for this, a second at most).
                    let mut n = 0u32;
                    // SAFETY: `h` is the connected pipe; one byte from a valid buffer.
                    unsafe { WriteFile(h, [ACK].as_ptr(), 1, &mut n, std::ptr::null_mut()) };
                    unsafe { FlushFileBuffers(h) };
                    // Which are folders is a disk question, which can be slow: not on this
                    // thread, which goes straight back to serving the next client.
                    if !paths.is_empty() {
                        let sink = sink.clone();
                        std::thread::spawn(move || sink(classify(paths, false), None));
                    }
                }
                // SAFETY: done with this client and this instance.
                unsafe {
                    DisconnectNamedPipe(h);
                    CloseHandle(h);
                }
                if next == INVALID_HANDLE_VALUE {
                    return;
                }
                h = next;
            }
        })
        .ok();
}

/// One instance of the pipe `name` (NUL-terminated); `first` fails if any exists already.
fn make(name: &[u16], first: bool) -> HANDLE {
    let mode = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
    // SAFETY: `name` is NUL-terminated; default security (this user and administrators may
    // write, others only read, which gives them at most the one-byte answer).
    unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            64,
            64 * 1024,
            0,
            std::ptr::null(),
        )
    }
}

/// Everything the client writes, until its request ends (or it closes its end; at most
/// 1 MB).
fn read_all(h: HANDLE) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    while out.len() < 1 << 20 && !complete(&out) {
        let mut n = 0u32;
        // SAFETY: `buf` is valid for its length.
        if unsafe { ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut()) } == 0 || n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_survive_the_pipe_encoding() {
        let paths = vec![PathBuf::from(r"C:\Users\me\Documents"), PathBuf::from(r"D:\été\naïve 2")];
        let bytes = encode(&paths);
        assert!(complete(&bytes) && !complete(&bytes[..bytes.len() - 2]), "the extra NUL ends it");
        assert_eq!(decode(&bytes), paths);
        assert!(decode(&[]).is_empty());
        assert_eq!(decode(&encode(&[PathBuf::new(), PathBuf::from(r"C:\")])), [PathBuf::from(r"C:\")], "an empty path can't end it early");
    }

    /// The review's case: a running Arbor that accepts the connection but never reads (hung,
    /// or stuck on an earlier request) used to hold a later launch forever. Now it gives
    /// up after the deadline and opens its own window.
    #[test]
    fn a_stalled_first_arbor_doesnt_hold_a_launch() {
        let name = format!(r"\\.\pipe\gg.smr.arbor.stalled.{}", std::process::id());
        let wname = wide(&name);
        let h = make(&wname, true) as usize;
        let (connected, hold) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            // SAFETY: the instance made above; it's never read from.
            unsafe { ConnectNamedPipe(h as HANDLE, std::ptr::null_mut()) };
            let _ = connected.send(());
            std::thread::sleep(std::time::Duration::from_secs(30));
        });
        let t = std::time::Instant::now();
        assert!(!hand_off(&name, &[PathBuf::from(r"C:\")]), "not taken");
        assert!(t.elapsed() < DEADLINE * 3, "gave up in {:?}", t.elapsed());
        drop(hold);
    }

    /// A second Arbor's hand-off reaches the first: each folder as a request, and the
    /// server keeps serving after a client.
    #[test]
    fn a_second_arbor_hands_its_folders_to_the_first() {
        let name = format!(r"\\.\pipe\gg.smr.arbor.test.{}", std::process::id());
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        serve(name.clone(), std::sync::Arc::new(move |shows, _| tx.lock().unwrap().send(shows).unwrap()));
        let a = std::env::temp_dir();
        let b = std::env::current_dir().unwrap();
        assert!(hand_off(&name, std::slice::from_ref(&a)));
        let got = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert_eq!(got, vec![super::super::Show::Folder(a)]);
        assert!(hand_off(&name, std::slice::from_ref(&b)), "still serving, right after the first");
        let got = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert_eq!(got, vec![super::super::Show::Folder(b.clone())]);
        // Several at once, back to back: none finds the pipe missing.
        for _ in 0..5 {
            assert!(hand_off(&name, std::slice::from_ref(&b)));
        }
        for _ in 0..5 {
            rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        }
        // A second server for the same name doesn't start; nobody serving: no hand-off.
        serve(name.clone(), std::sync::Arc::new(|_, _| panic!("a second server")));
        assert!(!hand_off(&format!("{name}.nobody"), &[PathBuf::from(r"C:\")]));
    }
}
