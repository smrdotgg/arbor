//! "Make Arbor the Default for Folders…" and "Restore the System Default for Folders…": the
//! opt-in that has other apps open folders in Arbor, and the command that undoes it. Neither
//! ever runs by itself: both go through Arbor's dialog (`confirm.rs`), which says what changes
//! and how to undo it.
//!
//! Before the first change, whatever it replaces is saved in `folder-handler-backup.txt` beside
//! the settings file (user data: the disk can't regenerate it). Restore puts exactly that back,
//! but only where the setting is still the one Arbor wrote: anything another app or the user
//! changed since is left alone and named. Without a backup, Restore still removes what Arbor
//! wrote. Making Arbor the default again while it already is keeps the first backup.
//!
//! - **macOS:** LaunchServices' viewer for `public.folder` (`LSSetDefaultRoleHandlerForContentType`),
//!   from the app bundle only. Restore sets the saved handler back, or Finder.
//! - **Linux:** `inode/directory=arbor.desktop` in `$XDG_CONFIG_HOME/mimeapps.list` (the
//!   freedesktop.org file `xdg-mime` and `gio` write), and a D-Bus activation file for
//!   `org.freedesktop.FileManager1`, so "Show in folder" from browsers and other apps starts
//!   Arbor when it isn't running (`incoming.rs` serves it).
//! - **Windows:** an "Open in Arbor" verb as the default for folders and drives, and Win+E,
//!   under `HKEY_CURRENT_USER` only (no administrator rights): see [`windows::plan`].
//!
//! It all runs on a worker (`State::change_default`): files, the registry and LaunchServices
//! are disk work.

use std::path::Path;

/// Which command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Make,
    Restore,
}

/// Beside the settings file.
pub const BACKUP_FILE: &str = "folder-handler-backup.txt";

/// Restore, when nothing was Arbor's.
const NOT_DEFAULT: &str = "Arbor isn't the default for folders, so there was nothing to restore";

/// Runs `change` for this platform. The message is for the status bar.
pub fn run(change: Change) -> Result<String, String> {
    if cfg!(test) || rune::tooling::headless_run().is_some() {
        return Err("The default for folders isn't changed in tests or headless runs".into());
    }
    let backup = crate::config::path().map(|p| p.with_file_name(BACKUP_FILE)).ok_or("There's no settings folder to keep the backup in")?;
    platform_run(change, &backup)
}

#[cfg(target_os = "macos")]
fn platform_run(change: Change, backup: &Path) -> Result<String, String> {
    mac::run(change, &mut mac::LaunchServices, mac::bundle_id().as_deref(), backup)
}

#[cfg(windows)]
fn platform_run(change: Change, backup: &Path) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("Can't tell where Arbor is: {e}"))?;
    let r = windows::run(change, &mut windows::Hkcu, windows::CLASSES, &exe.to_string_lossy(), backup);
    windows::notify_shell();
    r
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_run(change: Change, backup: &Path) -> Result<String, String> {
    linux::run(change, &linux::Dirs::from_env()?, backup)
}

// ---- the backup ---------------------------------------------------------------------------------

/// What a change replaced: named entries, each a value or "there was none".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Backup {
    entries: Vec<(String, Option<Vec<u8>>)>,
}

const BACKUP_HEADER: &str =
    "# What \"Make Arbor the Default for Folders\" replaced. \"Restore the System Default for Folders\" puts it back. Keep this file.";

impl Backup {
    /// `None` when nothing was recorded under `key`; `Some(None)` when it was recorded as absent.
    pub(crate) fn get(&self, key: &str) -> Option<Option<&[u8]>> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_deref())
    }

    pub(crate) fn put(&mut self, key: &str, value: Option<&[u8]>) {
        debug_assert!(!key.contains(['\n', '\r']));
        let value = value.map(<[u8]>::to_vec);
        match self.entries.iter_mut().find(|(k, _)| k == key) {
            Some(e) => e.1 = value,
            None => self.entries.push((key.to_string(), value)),
        }
    }

    /// One line per entry: the value in hex (or `-` for none), a tab, the name.
    fn encode(&self) -> String {
        let mut s = format!("{BACKUP_HEADER}\n");
        for (k, v) in &self.entries {
            let hex = v.as_ref().map_or_else(|| "-".to_string(), |b| b.iter().map(|x| format!("{x:02x}")).collect());
            s.push_str(&format!("{hex}\t{k}\n"));
        }
        s
    }

    fn decode(s: &str) -> Result<Backup, String> {
        // An empty or truncated file isn't a backup with nothing in it: it's damaged.
        if s.lines().next() != Some(BACKUP_HEADER) {
            return Err("it isn't a backup Arbor wrote (its first line is missing or changed)".into());
        }
        let mut b = Backup::default();
        for (n, line) in s.lines().enumerate() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let bad = || format!("line {} isn't what Arbor wrote", n + 1);
            let (hex, key) = line.split_once('\t').ok_or_else(bad)?;
            let value = if hex == "-" {
                None
            } else {
                if !hex.len().is_multiple_of(2) {
                    return Err(bad());
                }
                let bytes: Result<Vec<u8>, _> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16)).collect();
                Some(bytes.map_err(|_| bad())?)
            };
            if b.get(key).is_some() {
                return Err(format!("line {} records {key} again", n + 1));
            }
            b.entries.push((key.to_string(), value));
        }
        Ok(b)
    }

    /// The saved backup, or `None` when there's none.
    pub(crate) fn read(path: &Path) -> Result<Option<Backup>, String> {
        match std::fs::read_to_string(path) {
            Ok(s) => Backup::decode(&s).map(Some).map_err(|e| format!("{}: {e}, so nothing was changed", crate::fs::pretty(path))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", crate::fs::pretty(path))),
        }
    }

    pub(crate) fn write(&self, path: &Path) -> Result<(), String> {
        write_file(path, self.encode().as_bytes())
    }

    /// Checks that `backup` (read from `path`) records every one of `keys`, as every backup
    /// this platform's Make writes does; otherwise it's damaged, and nothing should be changed
    /// on its word.
    fn require(backup: Option<Backup>, keys: &[String], path: &Path) -> Result<Option<Backup>, String> {
        if let Some(b) = &backup
            && let Some(k) = keys.iter().find(|k| b.get(k).is_none())
        {
            return Err(format!(
                "{} isn't complete (it has no “{k}”), so nothing was changed. Put back a good copy, or delete it to have Restore take back only what Arbor wrote",
                crate::fs::pretty(path)
            ));
        }
        Ok(backup)
    }
}

/// Writes `data` to `path` durably ([`crate::persist::replace`]): a crash never leaves half a
/// file, and a backup is on the disk before anything it records is changed.
fn write_file(path: &Path, data: &[u8]) -> Result<(), String> {
    crate::persist::replace(path, data, crate::persist::Durability::Durable).map_err(|e| format!("{}: {e}", crate::fs::pretty(path)))
}

fn remove_backup(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {e}", crate::fs::pretty(path))),
        _ => Ok(()),
    }
}

/// "; x and y were left as they are" for settings changed since Arbor wrote them.
fn kept_note(kept: &[String]) -> String {
    match kept {
        [] => String::new(),
        [one] => format!(" ({one} was changed since, so it was left as it is)"),
        many => format!(" ({} were changed since, so they were left as they are)", many.join(", ")),
    }
}

// ---- Linux ----------------------------------------------------------------------------------

/// The freedesktop.org side. Plain files, so it builds and is tested on every platform.
#[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
pub(crate) mod linux;

// ---- Windows --------------------------------------------------------------------------------

/// The registry side: what to write, as data, so the plan and the backup and restore are
/// tested on every platform against an in-memory registry; [`Hkcu`] is the real one.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) mod windows;

// ---- macOS ----------------------------------------------------------------------------------

/// LaunchServices' viewer for folders. The logic is tested on every platform with a fake.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod mac;

#[cfg(test)]
mod tests;
