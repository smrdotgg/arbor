//! Open With: the apps that can open the selected file, the default first, and opening the
//! selection with the one chosen.
//!
//! - **macOS:** NSWorkspace's `URLsForApplicationsToOpenURL:` (Launch Services'
//!   `LSCopyApplicationURLsForURL` before macOS 12), the default from
//!   `URLForApplicationToOpenURL:`; opening runs `open -a`.
//! - **Linux:** the file's MIME type from `xdg-mime query filetype` when it's installed, else
//!   from its name through the shared MIME database's `globs2` (and a look at its first bytes
//!   for text), then the freedesktop.org associations: `mimeapps.list` in the spec's order
//!   (defaults, added and removed associations), each applications folder's `mimeinfo.cache`
//!   (or its `.desktop` files when it has none), and each app's `.desktop` file for its name
//!   and `Exec` line.
//! - **Windows:** the system's own Open With dialog (`SHOpenWithDialog`), which lists the apps,
//!   the default first, and lets you pick any other.
//!
//! Nothing is computed until it's asked for: the list is read on a worker when Open With opens
//! (the command, or the context menu on a file), and dropped when it closes. Opening runs off
//! the window's thread too.

use std::path::{Path, PathBuf};

use rune::{Key, KeyEvent, Latest, Tx};

use crate::app::State;

/// An app that can open the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handler {
    pub name: String,
    /// The app's bundle path (macOS) or desktop file id (Linux).
    pub id: String,
    /// It's what opens the file by default.
    pub default: bool,
    /// Linux: the desktop file's `Exec` line.
    pub exec: Option<String>,
}

/// The apps for `file`, the default first. Touches the disk: call on a worker.
pub fn list(file: &Path) -> Result<Vec<Handler>, String> {
    #[cfg(target_os = "macos")]
    {
        mac::list(file)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        linux::list(file)
    }
    #[cfg(windows)]
    {
        let _ = file;
        Err("Windows lists them in its own dialog".into())
    }
}

/// Opens `files` with `h`. Spawns the app and returns: call on a worker.
pub fn launch(h: &Handler, files: &[PathBuf]) -> Result<(), String> {
    if rune::tooling::headless_run().is_some() || files.is_empty() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let child = std::process::Command::new("open").arg("-a").arg(&h.id).args(files).spawn().map_err(|e| e.to_string())?;
        reap(child);
        Ok(())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let exec = h.exec.as_deref().ok_or("it has no Exec line")?;
        let runs = xdg::exec_argv(exec, files);
        if runs.is_empty() {
            return Err("its Exec line is empty".into());
        }
        for argv in runs {
            let child =
                std::process::Command::new(&argv[0]).args(&argv[1..]).spawn().map_err(|e| format!("{}: {e}", argv[0].to_string_lossy()))?;
            reap(child);
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = h;
        Err("use the system's Open With dialog".into())
    }
}

#[cfg(unix)]
fn reap(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

// ---- Linux: the shared MIME database and the freedesktop.org associations -------------------

/// The freedesktop.org pieces, kept apart so every platform's tests read them.
pub mod xdg;

#[cfg(all(unix, not(target_os = "macos")))]
mod linux;

// ---- macOS --------------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod mac;

// ---- Windows ------------------------------------------------------------------------------

/// Shows Windows' own Open With dialog for `file` (it lists the apps, the default first, and
/// "Choose another app"), on a thread of its own: it's modal and waits for the user.
#[cfg(windows)]
pub fn windows_dialog(file: PathBuf) {
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    use windows_sys::Win32::UI::Shell::{OAIF_ALLOW_REGISTRATION, OAIF_EXEC, OPENASINFO, SHOpenWithDialog};
    if rune::tooling::headless_run().is_some() {
        return;
    }
    std::thread::spawn(move || {
        let wide: Vec<u16> = file.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
        let info = OPENASINFO { pcszFile: wide.as_ptr(), pcszClass: std::ptr::null(), oaifInFlags: OAIF_ALLOW_REGISTRATION | OAIF_EXEC };
        // SAFETY: COM for this thread, then a dialog with a NUL-terminated path that outlives it.
        unsafe {
            CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
            SHOpenWithDialog(std::ptr::null_mut(), &info);
        }
    });
}

// ---- the picker ---------------------------------------------------------------------------

/// The Open With box, while it's open: the files it opens, and the apps once they're read.
pub struct OpenWithBox {
    pub files: Vec<PathBuf>,
    pub apps: Option<Result<Vec<Handler>, String>>,
    pub sel: usize,
    /// Which read is waited for.
    req: u64,
}

impl State {
    /// The files the selection holds (folders aren't opened with an app).
    fn open_with_files(&self) -> Vec<PathBuf> {
        let f = self.cur();
        f.targets().iter().filter(|e| !e.is_dir).map(|e| f.cwd.join(&e.os)).collect()
    }

    /// "Open With…": the box listing the apps for the selected files (read on a worker; the box
    /// waits at most a frame, then says it's looking). Windows: its own dialog.
    pub fn open_with(&mut self, tx: &mut Tx) {
        let files = self.open_with_files();
        let Some(first) = files.first().cloned() else { return };
        #[cfg(windows)]
        {
            let _ = tx;
            windows_dialog(first);
        }
        #[cfg(not(windows))]
        {
            self.close_overlays(tx);
            self.next_open_with += 1;
            let req = self.next_open_with;
            let apps = self.read_apps(req, first);
            self.open_with = Some(OpenWithBox { files, apps: apps.map(|(_, a)| a), sel: 0, req });
        }
    }

    /// Asks a worker for the apps for `file`, waiting at most a frame.
    fn read_apps(&mut self, req: u64, file: PathBuf) -> Option<(u64, Result<Vec<Handler>, String>)> {
        crate::bounded::read_waiting(&self.apps_loader, self.show_wait, move || Some((req, list(&file))))
    }

    /// The apps for a context menu on a file (`Some` only if they're read within a frame,
    /// which a native menu needs, since it can't change once open).
    pub(crate) fn apps_for_menu(&mut self) -> Option<Vec<Handler>> {
        if cfg!(windows) || !rune::popup_menu_supported() {
            return None;
        }
        let first = self.open_with_files().into_iter().next()?;
        self.next_open_with += 1;
        let req = self.next_open_with;
        self.read_apps(req, first).and_then(|(_, a)| a.ok())
    }

    pub fn close_open_with(&mut self) {
        self.open_with = None;
    }

    /// An app list that landed after the box's wait, from `poll`.
    pub(crate) fn poll_open_with(&mut self) -> bool {
        let mut changed = false;
        if let Some((req, apps)) = self.apps_loader.take()
            && let Some(b) = self.open_with.as_mut().filter(|b| b.req == req)
        {
            b.apps = Some(apps);
            changed = true;
        }
        if let Some(r) = self.launch_job.take() {
            if let Err(e) = r {
                self.note = Some(e);
                self.alert = self.note.clone();
            }
            changed = true;
        }
        changed
    }

    /// Opens the box's files with app `i`, off the window's thread, and closes the box.
    pub fn open_with_pick(&mut self, i: usize) {
        let Some(b) = self.open_with.take() else { return };
        let Some(Ok(apps)) = b.apps else { return };
        if let Some(h) = apps.get(i).cloned() {
            self.launch_with(h, b.files);
        }
    }

    /// Opens `files` with `h` on a worker; a failure lands in the status bar.
    pub(crate) fn launch_with(&mut self, h: Handler, files: Vec<PathBuf>) {
        let what = match files.as_slice() {
            [one] => format!("“{}”", one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
            many => format!("{} files", many.len()),
        };
        self.note = Some(format!("Opening {what} with {}", h.name));
        self.launch_job.request((h, files), |(h, files)| launch(&h, &files).map_err(|e| format!("Couldn't open with {}: {e}", h.name)));
    }

    /// The box's keys: ↑↓ move, Enter opens, Esc closes. It has every key while open.
    pub fn open_with_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        tx.instant();
        let Some(b) = self.open_with.as_mut() else { return false };
        let n = match &b.apps {
            Some(Ok(a)) => a.len(),
            _ => 0,
        };
        match ev.key {
            Key::Escape => self.open_with = None,
            Key::Down if n > 0 => b.sel = (b.sel + 1) % n,
            Key::Up if n > 0 => b.sel = (b.sel + n - 1) % n,
            Key::Home => b.sel = 0,
            Key::End => b.sel = n.saturating_sub(1),
            Key::Enter => {
                let i = b.sel;
                self.open_with_pick(i);
            }
            _ => {}
        }
        true
    }
}

/// A `Latest` for app lists, tagged with the request.
pub type AppsLoader = Latest<(u64, Result<Vec<Handler>, String>)>;

#[cfg(test)]
mod tests;
