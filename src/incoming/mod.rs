//! Folders other apps ask Arbor to show, whether it's running or they launch it.
//!
//! - **Windows and Linux** pass a folder on the command line (`arbor.exe "%1"`, the
//!   `.desktop` file's `%F`), which the first window opens (`app::start_dir`).
//! - **Windows**: a second `arbor.exe <folder>` hands its folders to the running Arbor over a
//!   named pipe and exits before opening a window ([`windows`]); the running one opens each in
//!   a new tab. A thread waits on the pipe (blocking, never polling).
//! - **macOS** sends an "open documents" Apple event instead, to the running app or the one it
//!   launched for it ("Open With → Arbor", `open -a Arbor dir`, Arbor as the default for
//!   folders). Rune passes on only "open URL" events (RUNE-GAPS.md), so Arbor installs its own
//!   handler, when AppKit says it's about to finish launching ([`mac`]).
//! - **Linux** also serves `org.freedesktop.FileManager1` on the session bus while Arbor is the
//!   default for folders ([`linux`]), so "Show in folder" from browsers and other apps opens a
//!   tab in the running Arbor, or the bus starts one through the activation file
//!   `handler::linux` writes. It's served only while that file is Arbor's: opting out stops it.
//!
//! A request becomes a tab ([`State::show_external`]): a folder opens in a new tab of the
//! active pane, and items open in their folder, selected. The first one replaces the tab the
//! window opened with while nobody has used it yet, so launching Arbor for a folder shows just
//! that folder. No polling: the bus thread waits on its socket, and AppKit calls the handler.

use std::ffi::OsString;
use std::path::PathBuf;

use rune::Tx;

use crate::app::State;
use crate::folder::Nav;

/// What another app asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Show {
    Folder(PathBuf),
    /// These names, selected in this folder.
    Items(PathBuf, Vec<OsString>),
}

/// Folders as themselves, anything else (or everything, with `items`) selected in its folder,
/// items of one folder together, in the order asked. Looks at the disk: off the UI thread.
pub fn classify(paths: Vec<PathBuf>, items: bool) -> Vec<Show> {
    let mut out: Vec<Show> = Vec::new();
    for p in paths {
        let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else {
            out.push(Show::Folder(p));
            continue;
        };
        if !items && p.is_dir() {
            out.push(Show::Folder(p));
            continue;
        }
        match out.iter_mut().find(|s| matches!(s, Show::Items(dir, _) if dir == parent)) {
            Some(Show::Items(_, names)) => names.push(name.to_os_string()),
            _ => out.push(Show::Items(parent.to_path_buf(), vec![name.to_os_string()])),
        }
    }
    out
}

#[cfg_attr(not(any(test, all(unix, not(target_os = "macos")))), allow(dead_code))]
/// The local path a `file://` URI names (`file:///a%20b` → `/a b`), or `None` for another
/// scheme or host.
pub fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let slash = rest.find('/')?;
    let (host, path) = rest.split_at(slash);
    if !host.is_empty() && host != "localhost" {
        return None;
    }
    let mut bytes = Vec::with_capacity(path.len());
    let b = path.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).ok()?, 16)
        {
            bytes.push(v);
            i += 3;
            continue;
        }
        bytes.push(b[i]);
        i += 1;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        Some(PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()))
    }
}

/// Where requests go: the window, through Rune's handle (any thread), with the activation
/// token the asking app passed, if any.
pub type Sink = std::sync::Arc<dyn Fn(Vec<Show>, Option<String>) + Send + Sync>;

/// Starts listening for requests to the running app (see the module docs). On Windows a thread
/// blocked on the pipe, a handler on macOS, and on Linux a thread that checks one file and ends
/// unless Arbor is the default for folders.
pub fn listen(handle: rune::AppHandle<State>) {
    let sink: Sink = std::sync::Arc::new(move |shows: Vec<Show>, token: Option<String>| {
        if shows.is_empty() {
            return;
        }
        handle.update(move |s, tx| {
            tx.instant();
            s.show_external(shows, tx);
        });
        match token.filter(|t| !t.is_empty()) {
            Some(t) => handle.activate(t),
            None => handle.open(),
        }
    });
    #[cfg(target_os = "macos")]
    mac::install(sink);
    #[cfg(all(unix, not(target_os = "macos")))]
    linux::spawn(sink);
    #[cfg(windows)]
    windows::serve(windows::pipe_name(), sink);
}

impl State {
    /// Shows what another app asked for: each folder in a new tab of the active pane, items
    /// selected in theirs. The first replaces the window's first tab while nobody has used it.
    pub fn show_external(&mut self, shows: Vec<Show>, tx: &mut Tx) {
        for show in shows {
            let (dir, select) = match show {
                Show::Folder(p) => (p, Vec::new()),
                Show::Items(p, names) => (p, names),
            };
            if !self.untouched() {
                self.new_tab(tx);
            }
            self.go_full(dir, Nav::Fresh, select, false);
            self.external_shows += 1;
        }
        self.sync_preview();
    }

    /// Whether the window is still as it opened: one tab, nothing navigated, selected,
    /// filtered or searched, and nothing shown for another app yet.
    fn untouched(&self) -> bool {
        let [pane] = self.panes.as_slice() else { return false };
        let f = &pane.f;
        self.external_shows == 0
            && pane.tabs.len() == 1
            && self.lands <= 1
            && f.back.is_empty()
            && f.fwd.is_empty()
            && f.picks == 0
            && f.filter.is_empty()
            && !f.searching()
    }
}

// ---- macOS ----------------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod mac;

// ---- Linux ----------------------------------------------------------------------------------

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) mod linux;

// ---- Windows --------------------------------------------------------------------------------

/// One Arbor per user session on Windows: the first serves a named pipe; a later
/// `arbor.exe <folder>` writes its folders to it and exits, and the first opens each in a tab.
/// Both ends block (the server on `ConnectNamedPipe`, which wakes when a client connects):
/// nothing polls. The pipe takes only local clients, and its default security lets only this
/// user (and administrators) write to it. Two Arbors started at the same instant may both
/// open windows (the second can't create the pipe, so it just runs on its own).
#[cfg(windows)]
pub(crate) mod windows;

#[cfg(test)]
mod tests;
