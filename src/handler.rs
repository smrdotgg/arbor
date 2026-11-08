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

use std::path::{Path, PathBuf};

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
            b.entries.push((key.to_string(), value));
        }
        Ok(b)
    }

    /// The saved backup, or `None` when there's none.
    pub(crate) fn read(path: &Path) -> Result<Option<Backup>, String> {
        match std::fs::read_to_string(path) {
            Ok(s) => Backup::decode(&s).map(Some).map_err(|e| format!("{}: {e}", crate::fs::pretty(path))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", crate::fs::pretty(path))),
        }
    }

    pub(crate) fn write(&self, path: &Path) -> Result<(), String> {
        write_file(path, self.encode().as_bytes())
    }
}

/// Writes `data` to `path` through a temporary file beside it, so a crash never leaves half a
/// file. Makes the folder if needed.
fn write_file(path: &Path, data: &[u8]) -> Result<(), String> {
    let say = |e: std::io::Error| format!("{}: {e}", crate::fs::pretty(path));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(say)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".arbor-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, data).map_err(say)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        say(e)
    })
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
pub(crate) mod linux {
    use super::{Backup, Change, NOT_DEFAULT, kept_note, remove_backup, write_file};
    use std::path::{Path, PathBuf};

    /// Installed by the tarball's `install.sh`.
    pub const DESKTOP_ID: &str = "arbor.desktop";
    pub const MIME: &str = "inode/directory";
    pub const BUS_NAME: &str = "org.freedesktop.FileManager1";
    /// The first line of the D-Bus activation file Arbor writes: how it knows the file is its own.
    pub const MARKER: &str = "# Written by Arbor";
    const SECTION: &str = "[Default Applications]";
    const MIME_KEY: &str = "mimeapps.list inode/directory";
    const SERVICE_KEY: &str = "dbus-1/services/org.freedesktop.FileManager1.service";

    /// Where the desktop keeps things, and Arbor's own path.
    pub struct Dirs {
        pub config_home: PathBuf,
        pub data_home: PathBuf,
        pub data_dirs: Vec<PathBuf>,
        pub exe: PathBuf,
        /// `XDG_CURRENT_DESKTOP`, lowercase (for `$desktop-mimeapps.list`).
        pub desktops: Vec<String>,
        /// Ask the session bus to read its service files again after a change (not in tests).
        pub reload_bus: bool,
    }

    fn env_dir(var: &str) -> Option<PathBuf> {
        std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute())
    }

    impl Dirs {
        pub fn from_env() -> Result<Dirs, String> {
            let home = env_dir("HOME").ok_or("HOME isn't set")?;
            let data_dirs = std::env::var("XDG_DATA_DIRS")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "/usr/local/share:/usr/share".into())
                .split(':')
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .collect();
            Ok(Dirs {
                config_home: env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")),
                data_home: data_home().ok_or("HOME isn't set")?,
                data_dirs,
                exe: std::env::current_exe().map_err(|e| format!("Can't tell where Arbor is: {e}"))?,
                desktops: std::env::var("XDG_CURRENT_DESKTOP")
                    .unwrap_or_default()
                    .split(':')
                    .filter(|s| !s.is_empty())
                    .map(str::to_lowercase)
                    .collect(),
                reload_bus: true,
            })
        }
    }

    /// `$XDG_DATA_HOME`, else `~/.local/share`.
    pub fn data_home() -> Option<PathBuf> {
        env_dir("XDG_DATA_HOME").or_else(|| env_dir("HOME").map(|h| h.join(".local/share")))
    }

    /// The session bus's activation file for the FileManager1 name, in the user's data folder.
    pub fn service_file(data_home: &Path) -> PathBuf {
        data_home.join(SERVICE_KEY)
    }

    /// Whether Arbor wrote this activation file (so it should serve the name).
    pub fn is_ours(text: &str) -> bool {
        text.starts_with(MARKER)
    }

    /// The activation file: the session bus runs Arbor when something asks for the name and
    /// nothing owns it.
    pub fn service_text(exe: &Path) -> String {
        format!(
            "{MARKER} (\"Make Arbor the Default for Folders\"). \"Restore the System Default for Folders\" removes it.\n[D-BUS Service]\nName={BUS_NAME}\nExec={}\n",
            quote(&exe.to_string_lossy())
        )
    }

    /// A path as the bus's `Exec=` reads it (shell-like quoting).
    fn quote(s: &str) -> String {
        if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+,:@".contains(c)) {
            return s.to_string();
        }
        let mut q = String::from("\"");
        for c in s.chars() {
            if matches!(c, '"' | '\\' | '$' | '`') {
                q.push('\\');
            }
            q.push(c);
        }
        q.push('"');
        q
    }

    /// Asks the session bus to read its configuration and service files again, so the
    /// activation file counts at once: a bus started before `~/.local/share/dbus-1/services`
    /// existed doesn't watch it, and would keep starting the old file manager until the next
    /// login. Best effort: without a bus there's nothing to tell.
    fn reload_bus() {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            rt.block_on(async {
                if let Ok(c) = zbus::Connection::session().await {
                    let _ = c
                        .call_method(
                            Some("org.freedesktop.DBus"),
                            "/org/freedesktop/DBus",
                            Some("org.freedesktop.DBus"),
                            "ReloadConfig",
                            &(),
                        )
                        .await;
                }
            });
        }
    }

    /// The value for `mime` in `[Default Applications]`, as written.
    pub fn default_app(text: &str, mime: &str) -> Option<String> {
        let mut inside = false;
        for line in text.lines() {
            let t = line.trim();
            if t.starts_with('[') {
                inside = t == SECTION;
            } else if inside
                && let Some((k, v)) = t.split_once('=')
                && k.trim() == mime
            {
                return Some(v.trim().to_string());
            }
        }
        None
    }

    /// `text` with `mime` set to `value` in `[Default Applications]` (or its line removed for
    /// `None`), every other line as it was.
    pub fn set_default_app(text: &str, mime: &str, value: Option<&str>) -> String {
        let mut out: Vec<String> = Vec::new();
        let (mut inside, mut done, mut seen) = (false, false, false);
        for line in text.lines() {
            let t = line.trim();
            if t.starts_with('[') {
                inside = t == SECTION;
                out.push(line.to_string());
                if inside {
                    seen = true;
                    if let (Some(v), false) = (value, done) {
                        out.push(format!("{mime}={v}"));
                        done = true;
                    }
                }
                continue;
            }
            if inside && t.split_once('=').is_some_and(|(k, _)| k.trim() == mime) {
                continue;
            }
            out.push(line.to_string());
        }
        if let (Some(v), false) = (value, seen) {
            if out.last().is_some_and(|l| !l.trim().is_empty()) {
                out.push(String::new());
            }
            out.push(SECTION.into());
            out.push(format!("{mime}={v}"));
        }
        let mut s = out.join("\n");
        if !s.is_empty() {
            s.push('\n');
        }
        s
    }

    fn first(value: &str) -> &str {
        value.split(';').next().unwrap_or("").trim()
    }

    fn read(path: &Path) -> Result<Option<String>, String> {
        match std::fs::read(path) {
            Ok(b) => Ok(Some(String::from_utf8_lossy(&b).into_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", crate::fs::pretty(path))),
        }
    }

    /// Where `arbor.desktop` is installed, if it is.
    fn desktop_file(d: &Dirs) -> Option<PathBuf> {
        std::iter::once(&d.data_home).chain(&d.data_dirs).map(|p| p.join("applications").join(DESKTOP_ID)).find(|p| p.is_file())
    }

    pub fn run(change: Change, d: &Dirs, backup: &Path) -> Result<String, String> {
        match change {
            Change::Make => make(d, backup),
            Change::Restore => restore(d, backup),
        }
    }

    fn make(d: &Dirs, backup: &Path) -> Result<String, String> {
        if desktop_file(d).is_none() {
            return Err(format!(
                "Install Arbor first: the download's install.sh puts {DESKTOP_ID} where the desktop finds it (~/.local/share/applications)"
            ));
        }
        let mimeapps = d.config_home.join("mimeapps.list");
        let text = read(&mimeapps)?.unwrap_or_default();
        let service = service_file(&d.data_home);
        let old_service = read(&service)?;
        if Backup::read(backup)?.is_none() {
            let mut b = Backup::default();
            b.put(MIME_KEY, default_app(&text, MIME).as_deref().map(str::as_bytes));
            b.put(SERVICE_KEY, old_service.as_deref().filter(|t| !is_ours(t)).map(str::as_bytes));
            b.write(backup)?;
        }
        write_file(&mimeapps, set_default_app(&text, MIME, Some(DESKTOP_ID)).as_bytes())?;
        write_file(&service, service_text(&d.exe).as_bytes())?;
        if d.reload_bus {
            reload_bus();
        }
        // A desktop's own list ($desktop-mimeapps.list) is read first and wins.
        let mut note = String::new();
        for name in &d.desktops {
            let p = d.config_home.join(format!("{name}-mimeapps.list"));
            if let Some(v) = read(&p).ok().flatten().and_then(|t| default_app(&t, MIME)).filter(|v| first(v) != DESKTOP_ID) {
                note = format!(", but {} names {} for folders and wins over it", crate::fs::pretty(&p), first(&v));
            }
        }
        Ok(format!(
            "Arbor is now the default for folders{note}. \"Show in folder\" in other apps opens it too. Restore the System Default for Folders undoes it."
        ))
    }

    fn restore(d: &Dirs, backup: &Path) -> Result<String, String> {
        let saved = Backup::read(backup)?;
        let b = saved.clone().unwrap_or_default();
        let (mut kept, mut changed) = (Vec::new(), false);
        let mimeapps = d.config_home.join("mimeapps.list");
        let before = b.get(MIME_KEY).flatten().map(|v| String::from_utf8_lossy(v).into_owned());
        if let Some(text) = read(&mimeapps)? {
            match default_app(&text, MIME) {
                Some(v) if first(&v) == DESKTOP_ID => {
                    write_file(&mimeapps, set_default_app(&text, MIME, before.as_deref()).as_bytes())?;
                    changed = true;
                }
                Some(_) => kept.push(format!("the folder default in {}", crate::fs::pretty(&mimeapps))),
                None => {}
            }
        }
        let service = service_file(&d.data_home);
        if let Some(text) = read(&service)? {
            if is_ours(&text) {
                match b.get(SERVICE_KEY).flatten() {
                    Some(old) => write_file(&service, old)?,
                    None => std::fs::remove_file(&service).map_err(|e| format!("{}: {e}", crate::fs::pretty(&service)))?,
                }
                changed = true;
            } else if saved.is_some() {
                kept.push(crate::fs::pretty(&service));
            }
        }
        remove_backup(backup)?;
        if changed && d.reload_bus {
            reload_bus();
        }
        if !changed && kept.is_empty() {
            return Ok(NOT_DEFAULT.into());
        }
        let to = before.as_deref().map_or_else(|| "the desktop's own file manager".to_string(), |v| first(v).to_string());
        Ok(format!("Folders open in {to} again{}", kept_note(&kept)))
    }
}

// ---- Windows --------------------------------------------------------------------------------

/// The registry side: what to write, as data, so the plan and the backup and restore are
/// tested on every platform against an in-memory registry; [`Hkcu`] is the real one.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) mod windows {
    use super::{Backup, Change, kept_note, remove_backup};
    use std::path::Path;

    /// The current user's classes (`HKEY_CURRENT_USER\Software\Classes`): no administrator
    /// rights, and nothing for other users.
    pub const CLASSES: &str = r"Software\Classes";
    /// The verb Arbor adds.
    pub const VERB: &str = "arbor";
    /// "This PC"'s class: its `opennewwindow` verb is what Win+E and the taskbar's Explorer
    /// button run.
    pub const WIN_E: &str = "{52205fd8-5dfb-447d-801a-d0b52f2e83e1}";
    pub const REG_SZ: u32 = 1;

    /// One string value to write: under `key` (below `HKEY_CURRENT_USER`), `name` ("" is the
    /// key's default value).
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Set {
        pub key: String,
        pub name: &'static str,
        pub data: String,
    }

    /// What "Make Arbor the Default for Folders" writes, the minimal set Explorer replacements
    /// use (Total Commander's and File Pilot's per-user recipes, Files' Win+E key):
    ///
    /// - `Directory\shell` and `Drive\shell` get a new verb, `arbor` ("Open in Arbor", running
    ///   `"arbor.exe" "%1"`), and it becomes their default verb. So opening a folder or a drive
    ///   anywhere (Explorer, the desktop, another app's ShellExecute) runs Arbor. Explorer's own
    ///   `open` verb isn't touched and stays in the context menu.
    /// - Win+E (`CLSID\{52205fd8…}\shell\opennewwindow\command`) runs Arbor, with an empty
    ///   `DelegateExecute` so the shell runs the command instead of Explorer's handler.
    ///
    /// Not `Folder`: it also covers virtual folders (Control Panel, Recycle Bin, libraries,
    /// phones) that Arbor can't show, which would stop opening at all.
    pub fn plan(root: &str, exe: &str) -> Vec<Set> {
        let set = |key: String, name: &'static str, data: String| Set { key, name, data };
        let mut v = Vec::new();
        for class in ["Directory", "Drive"] {
            let shell = format!(r"{root}\{class}\shell");
            v.push(set(shell.clone(), "", VERB.into()));
            v.push(set(format!(r"{shell}\{VERB}"), "", "Open in Arbor".into()));
            v.push(set(format!(r"{shell}\{VERB}"), "Icon", format!("{exe},0")));
            v.push(set(format!(r"{shell}\{VERB}\command"), "", format!("\"{exe}\" \"%1\"")));
        }
        let win_e = format!(r"{root}\CLSID\{WIN_E}\shell\opennewwindow\command");
        v.push(set(win_e.clone(), "", format!("\"{exe}\"")));
        v.push(set(win_e, "DelegateExecute", String::new()));
        v
    }

    /// Every key the plan writes to and the keys between them and `root`, parents first.
    pub fn keys(plan: &[Set], root: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for s in plan {
            let rest = s.key.strip_prefix(root).unwrap_or(&s.key).trim_start_matches('\\');
            let mut k = root.to_string();
            for part in rest.split('\\') {
                k = format!(r"{k}\{part}");
                if !out.contains(&k) {
                    out.push(k.clone());
                }
            }
        }
        out
    }

    /// The registry, as far as this needs it.
    pub trait Store {
        /// A value's type and bytes.
        fn get(&self, key: &str, name: &str) -> Option<(u32, Vec<u8>)>;
        /// Creates the key (and its parents) if needed.
        fn set(&mut self, key: &str, name: &str, kind: u32, data: &[u8]) -> Result<(), String>;
        fn delete_value(&mut self, key: &str, name: &str) -> Result<(), String>;
        fn exists(&self, key: &str) -> bool;
        /// Deletes the key only if it has no values and no subkeys left.
        fn delete_key_if_empty(&mut self, key: &str) -> Result<(), String>;
    }

    pub fn utf16z(s: &str) -> Vec<u8> {
        s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
    }

    /// A string value's text (without its terminating NULs).
    pub fn text(kind: u32, data: &[u8]) -> Option<String> {
        if kind != REG_SZ || !data.len().is_multiple_of(2) {
            return None;
        }
        let units: Vec<u16> = data.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        let end = units.iter().rposition(|&u| u != 0).map_or(0, |i| i + 1);
        String::from_utf16(&units[..end]).ok()
    }

    fn before_key(s: &Set) -> String {
        format!("before {}|{}", s.key, s.name)
    }

    fn wrote_key(s: &Set) -> String {
        format!("wrote {}|{}", s.key, s.name)
    }

    fn existed_key(k: &str) -> String {
        format!("existed {k}")
    }

    /// Writes `plan`. Records what it replaces first, unless `backup` already has it (Arbor was
    /// the default already: the first backup stays). Returns the backup to save.
    pub fn make(store: &mut dyn Store, plan: &[Set], root: &str, backup: Option<Backup>) -> Result<Backup, String> {
        let mut b = match backup {
            Some(b) => b,
            None => {
                let mut b = Backup::default();
                for k in keys(plan, root) {
                    b.put(&existed_key(&k), Some(if store.exists(&k) { b"1" } else { b"0" }));
                }
                for s in plan {
                    let old = store.get(&s.key, s.name).map(|(kind, data)| [kind.to_le_bytes().to_vec(), data].concat());
                    b.put(&before_key(s), old.as_deref());
                }
                b
            }
        };
        for s in plan {
            store.set(&s.key, s.name, REG_SZ, &utf16z(&s.data))?;
            b.put(&wrote_key(s), Some(s.data.as_bytes()));
        }
        Ok(b)
    }

    /// Puts back what `backup` recorded, for each value that still holds what Arbor wrote, then
    /// removes the keys Arbor made once they're empty. Without a backup, removes what this plan
    /// writes. Returns the values left alone because they changed since.
    pub fn restore(store: &mut dyn Store, plan: &[Set], root: &str, backup: Option<&Backup>) -> Result<(usize, Vec<String>), String> {
        let empty = Backup::default();
        let b = backup.unwrap_or(&empty);
        let (mut changed, mut kept) = (0, Vec::new());
        for s in plan.iter().rev() {
            let ours = b.get(&wrote_key(s)).flatten().map_or_else(|| s.data.clone(), |v| String::from_utf8_lossy(v).into_owned());
            match store.get(&s.key, s.name) {
                Some((kind, data)) if text(kind, &data).as_deref() == Some(ours.as_str()) => {
                    match b.get(&before_key(s)).flatten().filter(|v| v.len() >= 4) {
                        Some(old) => {
                            let kind = u32::from_le_bytes([old[0], old[1], old[2], old[3]]);
                            store.set(&s.key, s.name, kind, &old[4..])?;
                        }
                        None => store.delete_value(&s.key, s.name)?,
                    }
                    changed += 1;
                }
                Some(_) if backup.is_some() => {
                    let name = if s.name.is_empty() { "(Default)" } else { s.name };
                    kept.push(format!(r"HKCU\{}\{name}", s.key));
                }
                Some(_) => {}
                None => {}
            }
        }
        for k in keys(plan, root).iter().rev() {
            if b.get(&existed_key(k)).flatten() != Some(b"1") {
                store.delete_key_if_empty(k)?;
            }
        }
        Ok((changed, kept))
    }

    pub fn run(change: Change, store: &mut dyn Store, root: &str, exe: &str, backup: &Path) -> Result<String, String> {
        let plan = plan(root, exe);
        let saved = Backup::read(backup)?;
        match change {
            Change::Make => {
                let b = make(store, &plan, root, saved)?;
                b.write(backup)?;
                Ok("Arbor is now the default for folders and drives, and Win+E opens it. Restore the System Default for Folders undoes it."
                    .into())
            }
            Change::Restore => {
                let (changed, kept) = restore(store, &plan, root, saved.as_ref())?;
                remove_backup(backup)?;
                if changed == 0 && kept.is_empty() {
                    return Ok(super::NOT_DEFAULT.into());
                }
                Ok(format!("Folders open in Explorer again{}", kept_note(&kept)))
            }
        }
    }

    /// The current user's registry.
    #[cfg(windows)]
    pub struct Hkcu;

    #[cfg(windows)]
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[cfg(windows)]
    impl Hkcu {
        fn open(key: &str, access: u32) -> Option<windows_sys::Win32::System::Registry::HKEY> {
            use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RegOpenKeyExW};
            let mut h = std::ptr::null_mut();
            // SAFETY: a NUL-terminated key name and an out-pointer for the handle.
            (unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wide(key).as_ptr(), 0, access, &mut h) } == 0).then_some(h)
        }
    }

    #[cfg(windows)]
    fn fail(what: &str, key: &str, code: u32) -> String {
        format!(r"Couldn't {what} HKCU\{key}: {}", std::io::Error::from_raw_os_error(code as i32))
    }

    #[cfg(windows)]
    impl Store for Hkcu {
        fn get(&self, key: &str, name: &str) -> Option<(u32, Vec<u8>)> {
            use windows_sys::Win32::System::Registry::{KEY_READ, RegCloseKey, RegQueryValueExW};
            let h = Hkcu::open(key, KEY_READ)?;
            let n = wide(name);
            let (mut kind, mut len) = (0u32, 0u32);
            // SAFETY: the open key, a NUL-terminated name, and out-pointers; the second call
            // gets a buffer of the size the first reported.
            let r = unsafe {
                let mut r = RegQueryValueExW(h, n.as_ptr(), std::ptr::null(), &mut kind, std::ptr::null_mut(), &mut len);
                let mut data = vec![0u8; len as usize];
                if r == 0 && len > 0 {
                    r = RegQueryValueExW(h, n.as_ptr(), std::ptr::null(), &mut kind, data.as_mut_ptr(), &mut len);
                    data.truncate(len as usize);
                }
                RegCloseKey(h);
                (r == 0).then_some((kind, data))
            };
            r
        }

        fn set(&mut self, key: &str, name: &str, kind: u32, data: &[u8]) -> Result<(), String> {
            use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RegCloseKey, RegCreateKeyW, RegSetValueExW};
            let mut h = std::ptr::null_mut();
            // SAFETY: NUL-terminated names, the data's own length, and the handle closed after.
            unsafe {
                let r = RegCreateKeyW(HKEY_CURRENT_USER, wide(key).as_ptr(), &mut h);
                if r != 0 {
                    return Err(fail("create", key, r));
                }
                let r = RegSetValueExW(h, wide(name).as_ptr(), 0, kind, data.as_ptr(), data.len() as u32);
                RegCloseKey(h);
                if r != 0 {
                    return Err(fail("write", key, r));
                }
            }
            Ok(())
        }

        fn delete_value(&mut self, key: &str, name: &str) -> Result<(), String> {
            use windows_sys::Win32::System::Registry::{KEY_SET_VALUE, RegCloseKey, RegDeleteValueW};
            let Some(h) = Hkcu::open(key, KEY_SET_VALUE) else { return Ok(()) };
            // SAFETY: the open key and a NUL-terminated name; closed after.
            let r = unsafe {
                let r = RegDeleteValueW(h, wide(name).as_ptr());
                RegCloseKey(h);
                r
            };
            // 2: already gone.
            if r != 0 && r != 2 { Err(fail("change", key, r)) } else { Ok(()) }
        }

        fn exists(&self, key: &str) -> bool {
            use windows_sys::Win32::System::Registry::{KEY_READ, RegCloseKey};
            match Hkcu::open(key, KEY_READ) {
                Some(h) => {
                    // SAFETY: a handle just opened.
                    unsafe { RegCloseKey(h) };
                    true
                }
                None => false,
            }
        }

        fn delete_key_if_empty(&mut self, key: &str) -> Result<(), String> {
            use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, KEY_READ, RegCloseKey, RegDeleteKeyW, RegQueryInfoKeyW};
            let Some(h) = Hkcu::open(key, KEY_READ) else { return Ok(()) };
            let (mut subkeys, mut values) = (0u32, 0u32);
            let null = std::ptr::null_mut();
            // SAFETY: the open key; only the counts are asked for, the rest are null.
            let r = unsafe {
                let r = RegQueryInfoKeyW(h, null, null, std::ptr::null(), &mut subkeys, null, null, &mut values, null, null, null, null);
                RegCloseKey(h);
                r
            };
            if r != 0 || subkeys != 0 || values != 0 {
                return Ok(());
            }
            // SAFETY: a NUL-terminated key name.
            let r = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, wide(key).as_ptr()) };
            if r != 0 && r != 2 { Err(fail("remove", key, r)) } else { Ok(()) }
        }
    }

    /// Tells Explorer the associations changed, so it doesn't wait for a restart.
    #[cfg(windows)]
    pub fn notify_shell() {
        use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
        // SAFETY: no items are passed for this event.
        unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
    }
}

// ---- macOS ----------------------------------------------------------------------------------

/// LaunchServices' viewer for folders. The logic is tested on every platform with a fake.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod mac {
    use super::{Backup, Change, remove_backup};
    use std::path::Path;

    pub const BUNDLE_ID: &str = "gg.smr.arbor";
    pub const FINDER: &str = "com.apple.finder";
    pub const FOLDER_TYPE: &str = "public.folder";
    const BEFORE: &str = "public.folder viewer";

    /// The default viewer for `public.folder`, as far as this needs it.
    pub trait Handlers {
        /// What LaunchServices answers now. Without a choice of the user's, that's a guess
        /// among the apps that declare folders (Archive Utility, an editor), never Finder,
        /// which handles folders without declaring them.
        fn current(&self) -> Option<String>;
        /// The viewer the user (or an app) chose explicitly, if any: the one worth putting back.
        fn chosen(&self) -> Option<String>;
        fn set(&mut self, bundle_id: &str) -> Result<(), String>;
    }

    /// `bundle` is the running app's bundle id, if it runs from a bundle.
    pub fn run(change: Change, ls: &mut dyn Handlers, bundle: Option<&str>, backup: &Path) -> Result<String, String> {
        match change {
            Change::Make => {
                if bundle != Some(BUNDLE_ID) {
                    return Err("Only Arbor.app can be the default for folders: macOS asks the app bundle (this Arbor runs as a bare program). Open Arbor.app and run it there".into());
                }
                if Backup::read(backup)?.is_none() {
                    let mut b = Backup::default();
                    b.put(BEFORE, ls.chosen().filter(|c| !c.eq_ignore_ascii_case(BUNDLE_ID)).as_deref().map(str::as_bytes));
                    b.write(backup)?;
                }
                ls.set(BUNDLE_ID)?;
                Ok("Arbor is now the default for folders that apps open. Finder still opens the desktop, the Dock's folders and \"Show in Finder\" from other apps. Restore the System Default for Folders undoes it.".into())
            }
            Change::Restore => {
                let b = Backup::read(backup)?.unwrap_or_default();
                let to = b.get(BEFORE).flatten().map_or_else(|| FINDER.to_string(), |v| String::from_utf8_lossy(v).into_owned());
                let now = ls.current();
                if now.as_deref().is_some_and(|c| !c.eq_ignore_ascii_case(BUNDLE_ID)) {
                    remove_backup(backup)?;
                    return Ok(format!(
                        "Arbor isn't the default for folders ({} is, set since), so nothing was changed",
                        now.unwrap_or_default()
                    ));
                }
                ls.set(&to)?;
                remove_backup(backup)?;
                let name = if to == FINDER { "Finder".to_string() } else { to };
                Ok(format!("Folders open in {name} again"))
            }
        }
    }

    /// The running app's bundle id, when it runs from an `.app` bundle.
    #[cfg(target_os = "macos")]
    pub fn bundle_id() -> Option<String> {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;
        use objc2_foundation::NSString;
        objc2::rc::autoreleasepool(|_| {
            // SAFETY: class messages returning autoreleased objects (or nil), released with the pool.
            unsafe {
                let bundle: *mut AnyObject = msg_send![objc2::class!(NSBundle), mainBundle];
                let bundle = bundle.as_ref()?;
                let path: *mut NSString = msg_send![bundle, bundlePath];
                if !path.as_ref()?.to_string().ends_with(".app") {
                    return None;
                }
                let id: *mut NSString = msg_send![bundle, bundleIdentifier];
                Some(id.as_ref()?.to_string())
            }
        })
    }

    #[cfg(target_os = "macos")]
    pub struct LaunchServices;

    #[cfg(target_os = "macos")]
    mod ffi {
        use std::ffi::c_void;
        /// `kLSRolesViewer`.
        pub const VIEWER: u32 = 2;
        #[link(name = "CoreServices", kind = "framework")]
        unsafe extern "C" {
            pub fn LSCopyDefaultRoleHandlerForContentType(content_type: *const c_void, role: u32) -> *const c_void;
            pub fn LSSetDefaultRoleHandlerForContentType(content_type: *const c_void, role: u32, handler: *const c_void) -> i32;
        }
        #[link(name = "CoreFoundation", kind = "framework")]
        unsafe extern "C" {
            pub fn CFRelease(cf: *const c_void);
        }
    }

    /// The `public.folder` viewer in LaunchServices' own record of explicit choices
    /// (`LSHandlers` in `com.apple.launchservices.secure.plist`). Read only.
    #[cfg(target_os = "macos")]
    fn chosen_viewer() -> Option<String> {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;
        use objc2_foundation::NSString;
        let home = std::env::var("HOME").ok()?;
        let file = format!("{home}/Library/Preferences/com.apple.LaunchServices/com.apple.launchservices.secure.plist");
        objc2::rc::autoreleasepool(|_| {
            let string = |o: *mut AnyObject| -> Option<String> {
                // SAFETY: `o` is nil or an object; it's read only if it's an NSString.
                unsafe {
                    let o = o.as_ref()?;
                    let is: bool = msg_send![o, isKindOfClass: objc2::class!(NSString)];
                    is.then(|| (*(o as *const AnyObject).cast::<NSString>()).to_string())
                }
            };
            let key = |k: &str| NSString::from_str(k);
            // SAFETY: Foundation reads of a property list; every object is checked before use.
            unsafe {
                let dict: *mut AnyObject = msg_send![objc2::class!(NSDictionary), dictionaryWithContentsOfFile: &*key(&file)];
                let dict = dict.as_ref()?;
                let handlers: *mut AnyObject = msg_send![dict, objectForKey: &*key("LSHandlers")];
                let handlers = handlers.as_ref()?;
                let is_array: bool = msg_send![handlers, isKindOfClass: objc2::class!(NSArray)];
                if !is_array {
                    return None;
                }
                let n: usize = msg_send![handlers, count];
                for i in 0..n {
                    let h: *mut AnyObject = msg_send![handlers, objectAtIndex: i];
                    let Some(h) = h.as_ref() else { continue };
                    let is_dict: bool = msg_send![h, isKindOfClass: objc2::class!(NSDictionary)];
                    if !is_dict || string(msg_send![h, objectForKey: &*key("LSHandlerContentType")]).as_deref() != Some(FOLDER_TYPE) {
                        continue;
                    }
                    return string(msg_send![h, objectForKey: &*key("LSHandlerRoleViewer")])
                        .or_else(|| string(msg_send![h, objectForKey: &*key("LSHandlerRoleAll")]))
                        .filter(|v| v != "-");
                }
                None
            }
        })
    }

    #[cfg(target_os = "macos")]
    impl Handlers for LaunchServices {
        fn chosen(&self) -> Option<String> {
            chosen_viewer()
        }

        fn current(&self) -> Option<String> {
            use objc2_foundation::NSString;
            let ty = NSString::from_str(FOLDER_TYPE);
            // SAFETY: an NSString is a CFString (toll-free bridged); the copied result is
            // released after it's read.
            unsafe {
                let got = ffi::LSCopyDefaultRoleHandlerForContentType(objc2::rc::Retained::as_ptr(&ty).cast(), ffi::VIEWER);
                if got.is_null() {
                    return None;
                }
                let s = (*got.cast::<NSString>()).to_string();
                ffi::CFRelease(got);
                Some(s)
            }
        }

        fn set(&mut self, bundle_id: &str) -> Result<(), String> {
            use objc2_foundation::NSString;
            let (ty, id) = (NSString::from_str(FOLDER_TYPE), NSString::from_str(bundle_id));
            // SAFETY: two CFStrings (bridged NSStrings) alive for the call.
            let r = unsafe {
                ffi::LSSetDefaultRoleHandlerForContentType(
                    objc2::rc::Retained::as_ptr(&ty).cast(),
                    ffi::VIEWER,
                    objc2::rc::Retained::as_ptr(&id).cast(),
                )
            };
            if r == 0 { Ok(()) } else { Err(format!("macOS refused to change the default for folders (error {r})")) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-handler-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_backup_reads_back_what_it_wrote() {
        let mut b = Backup::default();
        b.put("a key with spaces|Icon", Some(b"\x00\x01text\xff"));
        b.put("absent", None);
        b.put("a key with spaces|Icon", Some(b"again"));
        let d = tmp("backup");
        let p = d.join(BACKUP_FILE);
        b.write(&p).unwrap();
        let back = Backup::read(&p).unwrap().unwrap();
        assert_eq!(back, b);
        assert_eq!(back.get("absent"), Some(None));
        assert_eq!(back.get("missing"), None);
        assert_eq!(back.get("a key with spaces|Icon"), Some(Some(&b"again"[..])));
        assert!(Backup::read(&d.join("none")).unwrap().is_none());
        std::fs::write(&p, "zz\tkey\n").unwrap();
        assert!(Backup::read(&p).is_err(), "a damaged backup is said, not guessed at");
        let _ = std::fs::remove_dir_all(&d);
    }

    // ---- Linux ----

    fn linux_dirs(d: &Path, installed: bool) -> linux::Dirs {
        let data_home = d.join("data");
        if installed {
            std::fs::create_dir_all(data_home.join("applications")).unwrap();
            std::fs::write(data_home.join("applications/arbor.desktop"), "[Desktop Entry]\n").unwrap();
        }
        linux::Dirs {
            config_home: d.join("config"),
            data_home,
            data_dirs: vec![d.join("system")],
            exe: PathBuf::from("/home/me/.local/bin/arbor"),
            desktops: vec!["gnome".into()],
            reload_bus: false,
        }
    }

    #[test]
    fn mimeapps_edits_keep_every_other_line() {
        let text = "[Added Associations]\ninode/directory=x.desktop;\n\n[Default Applications]\ntext/plain=gedit.desktop\ninode/directory = org.gnome.Nautilus.desktop\n# note\n";
        assert_eq!(linux::default_app(text, "inode/directory").as_deref(), Some("org.gnome.Nautilus.desktop"));
        let set = linux::set_default_app(text, "inode/directory", Some("arbor.desktop"));
        assert_eq!(
            set,
            "[Added Associations]\ninode/directory=x.desktop;\n\n[Default Applications]\ninode/directory=arbor.desktop\ntext/plain=gedit.desktop\n# note\n"
        );
        let removed = linux::set_default_app(&set, "inode/directory", None);
        assert_eq!(
            removed,
            "[Added Associations]\ninode/directory=x.desktop;\n\n[Default Applications]\ntext/plain=gedit.desktop\n# note\n"
        );
        assert_eq!(
            linux::set_default_app("", "inode/directory", Some("arbor.desktop")),
            "[Default Applications]\ninode/directory=arbor.desktop\n"
        );
        assert_eq!(
            linux::set_default_app("[Other]\na=b", "inode/directory", Some("arbor.desktop")),
            "[Other]\na=b\n\n[Default Applications]\ninode/directory=arbor.desktop\n"
        );
    }

    #[test]
    fn linux_make_and_restore_put_back_exactly_what_was_there() {
        let d = tmp("linux-roundtrip");
        let dirs = linux_dirs(&d, true);
        let backup = d.join("arbor").join(BACKUP_FILE);
        let mimeapps = dirs.config_home.join("mimeapps.list");
        let service = linux::service_file(&dirs.data_home);
        let before = "[Default Applications]\ninode/directory=org.gnome.Nautilus.desktop\ntext/plain=gedit.desktop\n";
        std::fs::create_dir_all(&dirs.config_home).unwrap();
        std::fs::write(&mimeapps, before).unwrap();
        std::fs::create_dir_all(service.parent().unwrap()).unwrap();
        std::fs::write(&service, "[D-BUS Service]\nName=org.freedesktop.FileManager1\nExec=/usr/bin/other\n").unwrap();
        let msg = linux::run(Change::Make, &dirs, &backup).unwrap();
        assert!(msg.starts_with("Arbor is now the default for folders"), "{msg}");
        assert_eq!(linux::default_app(&std::fs::read_to_string(&mimeapps).unwrap(), linux::MIME).as_deref(), Some("arbor.desktop"));
        let s = std::fs::read_to_string(&service).unwrap();
        assert!(
            linux::is_ours(&s) && s.contains("Exec=/home/me/.local/bin/arbor\n") && s.contains("Name=org.freedesktop.FileManager1"),
            "{s}"
        );
        // Again: the first backup stays (it isn't overwritten with Arbor's own settings).
        linux::run(Change::Make, &dirs, &backup).unwrap();
        let msg = linux::run(Change::Restore, &dirs, &backup).unwrap();
        assert_eq!(msg, "Folders open in org.gnome.Nautilus.desktop again");
        assert_eq!(std::fs::read_to_string(&mimeapps).unwrap(), before);
        assert_eq!(std::fs::read_to_string(&service).unwrap(), "[D-BUS Service]\nName=org.freedesktop.FileManager1\nExec=/usr/bin/other\n");
        assert!(!backup.exists(), "the backup goes once it's put back");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn linux_restore_with_nothing_before_removes_what_arbor_wrote() {
        let d = tmp("linux-fresh");
        let dirs = linux_dirs(&d, true);
        let backup = d.join("arbor").join(BACKUP_FILE);
        linux::run(Change::Make, &dirs, &backup).unwrap();
        let msg = linux::run(Change::Restore, &dirs, &backup).unwrap();
        assert_eq!(msg, "Folders open in the desktop's own file manager again");
        let text = std::fs::read_to_string(dirs.config_home.join("mimeapps.list")).unwrap();
        assert_eq!(linux::default_app(&text, linux::MIME), None, "{text}");
        assert!(!linux::service_file(&dirs.data_home).exists());
        // Without a backup at all, Restore still takes back what Arbor wrote.
        linux::run(Change::Make, &dirs, &backup).unwrap();
        std::fs::remove_file(&backup).unwrap();
        linux::run(Change::Restore, &dirs, &backup).unwrap();
        assert!(!linux::service_file(&dirs.data_home).exists());
        assert_eq!(linux::run(Change::Restore, &dirs, &backup).unwrap(), NOT_DEFAULT);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn linux_restore_leaves_settings_changed_since_and_make_needs_the_desktop_file() {
        let d = tmp("linux-changed");
        let dirs = linux_dirs(&d, false);
        let backup = d.join("arbor").join(BACKUP_FILE);
        let e = linux::run(Change::Make, &dirs, &backup).unwrap_err();
        assert!(e.starts_with("Install Arbor first"), "{e}");
        assert!(!backup.exists() && !dirs.config_home.join("mimeapps.list").exists(), "nothing changed");
        let dirs = linux_dirs(&d, true);
        linux::run(Change::Make, &dirs, &backup).unwrap();
        let mimeapps = dirs.config_home.join("mimeapps.list");
        std::fs::write(&mimeapps, "[Default Applications]\ninode/directory=thunar.desktop\n").unwrap();
        let msg = linux::run(Change::Restore, &dirs, &backup).unwrap();
        assert!(msg.contains("was changed since, so it was left as it is"), "{msg}");
        assert_eq!(std::fs::read_to_string(&mimeapps).unwrap(), "[Default Applications]\ninode/directory=thunar.desktop\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_desktops_own_list_that_wins_is_said() {
        let d = tmp("linux-desktop-list");
        let dirs = linux_dirs(&d, true);
        std::fs::create_dir_all(&dirs.config_home).unwrap();
        std::fs::write(dirs.config_home.join("gnome-mimeapps.list"), "[Default Applications]\ninode/directory=nautilus.desktop\n").unwrap();
        let msg = linux::run(Change::Make, &dirs, &d.join(BACKUP_FILE)).unwrap();
        assert!(msg.contains("gnome-mimeapps.list names nautilus.desktop for folders and wins over it"), "{msg}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_service_files_exec_is_quoted_for_the_bus() {
        let s = linux::service_text(Path::new("/opt/my apps/arbor"));
        assert!(s.contains("Exec=\"/opt/my apps/arbor\"\n"), "{s}");
    }

    // ---- Windows ----

    /// An in-memory registry.
    #[derive(Default, Debug, Clone, PartialEq)]
    struct Reg {
        keys: BTreeMap<String, BTreeMap<String, (u32, Vec<u8>)>>,
    }

    impl Reg {
        fn create(&mut self, key: &str) {
            let mut k = String::new();
            for part in key.split('\\') {
                if !k.is_empty() {
                    k.push('\\');
                }
                k.push_str(part);
                self.keys.entry(k.to_lowercase()).or_default();
            }
        }
        fn text(&self, key: &str, name: &str) -> Option<String> {
            windows::Store::get(self, key, name).and_then(|(k, d)| windows::text(k, &d))
        }
    }

    impl windows::Store for Reg {
        fn get(&self, key: &str, name: &str) -> Option<(u32, Vec<u8>)> {
            self.keys.get(&key.to_lowercase())?.get(&name.to_lowercase()).cloned()
        }
        fn set(&mut self, key: &str, name: &str, kind: u32, data: &[u8]) -> Result<(), String> {
            self.create(key);
            self.keys.get_mut(&key.to_lowercase()).unwrap().insert(name.to_lowercase(), (kind, data.to_vec()));
            Ok(())
        }
        fn delete_value(&mut self, key: &str, name: &str) -> Result<(), String> {
            if let Some(k) = self.keys.get_mut(&key.to_lowercase()) {
                k.remove(&name.to_lowercase());
            }
            Ok(())
        }
        fn exists(&self, key: &str) -> bool {
            self.keys.contains_key(&key.to_lowercase())
        }
        fn delete_key_if_empty(&mut self, key: &str) -> Result<(), String> {
            let k = key.to_lowercase();
            let prefix = format!("{k}\\");
            if self.keys.get(&k).is_some_and(|v| v.is_empty()) && !self.keys.keys().any(|o| o.starts_with(&prefix)) {
                self.keys.remove(&k);
            }
            Ok(())
        }
    }

    const EXE: &str = r"C:\Users\me\AppData\Local\Arbor\arbor.exe";

    #[test]
    fn windows_plan_is_the_minimal_hkcu_set() {
        let plan = windows::plan(windows::CLASSES, EXE);
        let lines: Vec<String> = plan.iter().map(|s| format!(r"{}\[{}] = {}", s.key, s.name, s.data)).collect();
        assert_eq!(
            lines,
            [
                r"Software\Classes\Directory\shell\[] = arbor",
                r"Software\Classes\Directory\shell\arbor\[] = Open in Arbor",
                r"Software\Classes\Directory\shell\arbor\[Icon] = C:\Users\me\AppData\Local\Arbor\arbor.exe,0",
                r#"Software\Classes\Directory\shell\arbor\command\[] = "C:\Users\me\AppData\Local\Arbor\arbor.exe" "%1""#,
                r"Software\Classes\Drive\shell\[] = arbor",
                r"Software\Classes\Drive\shell\arbor\[] = Open in Arbor",
                r"Software\Classes\Drive\shell\arbor\[Icon] = C:\Users\me\AppData\Local\Arbor\arbor.exe,0",
                r#"Software\Classes\Drive\shell\arbor\command\[] = "C:\Users\me\AppData\Local\Arbor\arbor.exe" "%1""#,
                r#"Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow\command\[] = "C:\Users\me\AppData\Local\Arbor\arbor.exe""#,
                r"Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow\command\[DelegateExecute] = ",
            ]
        );
        assert!(plan.iter().all(|s| !s.key.to_lowercase().contains(r"classes\folder")), "never the Folder class");
        let keys = windows::keys(&plan, windows::CLASSES);
        assert_eq!(keys[0], r"Software\Classes\Directory");
        assert!(keys.contains(&r"Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow".to_string()));
    }

    #[test]
    fn windows_make_and_restore_put_back_exactly_what_was_there() {
        let root = windows::CLASSES;
        let mut reg = Reg::default();
        use windows::Store;
        // A user who had another verb as the default for folders, and a CLSID key already.
        reg.set(r"Software\Classes\Directory\shell", "", 2, &windows::utf16z("%SystemRoot%\\other")).unwrap();
        reg.set(r"Software\Classes\Directory\shell\find", "", windows::REG_SZ, &windows::utf16z("Find")).unwrap();
        reg.create(r"Software\Classes\CLSID");
        let start = reg.clone();
        let plan = windows::plan(root, EXE);
        let b = windows::make(&mut reg, &plan, root, None).unwrap();
        assert_eq!(reg.text(r"Software\Classes\Directory\shell", "").as_deref(), Some("arbor"));
        assert_eq!(reg.text(r"Software\Classes\Drive\shell\arbor\command", "").as_deref(), Some(&*format!("\"{EXE}\" \"%1\"")));
        // Made twice: the first backup is kept.
        let b = windows::make(&mut reg, &plan, root, Some(b)).unwrap();
        let (changed, kept) = windows::restore(&mut reg, &plan, root, Some(&b)).unwrap();
        assert!(kept.is_empty() && changed == plan.len(), "{kept:?}");
        assert_eq!(reg, start, "everything as it was, the expandable string's type too");
        assert_eq!(windows::restore(&mut reg, &plan, root, None).unwrap(), (0, Vec::new()), "nothing of Arbor's left");
    }

    #[test]
    fn windows_restore_without_a_backup_and_after_changes() {
        let root = windows::CLASSES;
        let mut reg = Reg::default();
        use windows::Store;
        let plan = windows::plan(root, EXE);
        windows::make(&mut reg, &plan, root, None).unwrap();
        // Another app took over drives since.
        reg.set(r"Software\Classes\Drive\shell", "", windows::REG_SZ, &windows::utf16z("other")).unwrap();
        let b = windows::make(&mut Reg::default(), &plan, root, None).unwrap();
        let (_, kept) = windows::restore(&mut reg, &plan, root, Some(&b)).unwrap();
        assert_eq!(kept, [r"HKCU\Software\Classes\Drive\shell\(Default)"]);
        assert_eq!(reg.text(r"Software\Classes\Drive\shell", "").as_deref(), Some("other"));
        assert!(!reg.exists(r"Software\Classes\Directory"), "keys Arbor made are gone: {reg:?}");
        assert!(!reg.exists(r"Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}"));
        assert!(!reg.exists(r"Software\Classes\Drive\shell\arbor"));
        // With no backup at all, what Arbor wrote goes.
        let mut reg = Reg::default();
        reg.create(root);
        let start = reg.clone();
        windows::make(&mut reg, &plan, root, None).unwrap();
        assert_eq!(windows::restore(&mut reg, &plan, root, None).unwrap(), (plan.len(), Vec::new()));
        assert_eq!(reg, start);
        // An Arbor that moved since is still recognised: what it wrote is in the backup.
        let mut reg = Reg::default();
        reg.create(root);
        let start = reg.clone();
        let b = windows::make(&mut reg, &plan, root, None).unwrap();
        let moved = windows::plan(root, r"D:\Arbor\arbor.exe");
        assert!(windows::restore(&mut reg, &moved, root, Some(&b)).unwrap().1.is_empty());
        assert_eq!(reg, start);
    }

    /// The real registry, under a test-only key (never the real classes): write, check, restore.
    #[cfg(windows)]
    #[test]
    fn windows_round_trip_in_the_real_registry_under_a_test_key() {
        use windows::Store;
        let root = format!(r"Software\ArborTest-{}", std::process::id());
        let mut reg = windows::Hkcu;
        reg.set(&format!(r"{root}\Directory\shell"), "", windows::REG_SZ, &windows::utf16z("before")).unwrap();
        let plan = windows::plan(&root, EXE);
        let b = windows::make(&mut reg, &plan, &root, None).unwrap();
        let read = |k: &str, n: &str| reg.get(k, n).and_then(|(kind, d)| windows::text(kind, &d));
        assert_eq!(read(&format!(r"{root}\Directory\shell"), "").as_deref(), Some("arbor"));
        assert_eq!(read(&format!(r"{root}\Drive\shell\arbor\command"), "").as_deref(), Some(&*format!("\"{EXE}\" \"%1\"")));
        assert_eq!(read(&format!(r"{root}\CLSID\{}\shell\opennewwindow\command", windows::WIN_E), "DelegateExecute").as_deref(), Some(""));
        let (_, kept) = windows::restore(&mut reg, &plan, &root, Some(&b)).unwrap();
        assert!(kept.is_empty(), "{kept:?}");
        let read = |k: &str, n: &str| reg.get(k, n).and_then(|(kind, d)| windows::text(kind, &d));
        assert_eq!(read(&format!(r"{root}\Directory\shell"), "").as_deref(), Some("before"));
        assert!(!reg.exists(&format!(r"{root}\Drive")) && !reg.exists(&format!(r"{root}\CLSID")));
        assert!(!reg.exists(&format!(r"{root}\Directory\shell\arbor")));
        reg.delete_value(&format!(r"{root}\Directory\shell"), "").unwrap();
        for k in [format!(r"{root}\Directory\shell"), format!(r"{root}\Directory"), root.clone()] {
            reg.delete_key_if_empty(&k).unwrap();
        }
        assert!(!reg.exists(&root));
    }

    // ---- macOS ----

    /// LaunchServices: what it answers now, and whether that was chosen (or is its guess).
    struct FakeLs(Option<String>, bool);
    impl mac::Handlers for FakeLs {
        fn current(&self) -> Option<String> {
            self.0.clone()
        }
        fn chosen(&self) -> Option<String> {
            self.0.clone().filter(|_| self.1)
        }
        fn set(&mut self, id: &str) -> Result<(), String> {
            self.0 = Some(id.into());
            self.1 = true;
            Ok(())
        }
    }

    #[test]
    fn mac_make_needs_the_bundle_and_restore_puts_back_the_old_viewer() {
        let d = tmp("mac");
        let backup = d.join(BACKUP_FILE);
        let mut ls = FakeLs(Some("com.cocoatech.PathFinder".into()), true);
        let e = mac::run(Change::Make, &mut ls, None, &backup).unwrap_err();
        assert!(e.starts_with("Only Arbor.app can be the default for folders"), "{e}");
        assert_eq!(ls.0.as_deref(), Some("com.cocoatech.PathFinder"), "nothing changed");
        mac::run(Change::Make, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap();
        assert_eq!(ls.0.as_deref(), Some(mac::BUNDLE_ID));
        mac::run(Change::Make, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap();
        assert_eq!(
            mac::run(Change::Restore, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap(),
            "Folders open in com.cocoatech.PathFinder again"
        );
        assert_eq!(ls.0.as_deref(), Some("com.cocoatech.PathFinder"));
        // Only LaunchServices' guess (Archive Utility, as on the Mac here): Finder, never the guess.
        let mut ls = FakeLs(Some("com.apple.archiveutility".into()), false);
        mac::run(Change::Make, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap();
        assert_eq!(mac::run(Change::Restore, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap(), "Folders open in Finder again");
        assert_eq!(ls.0.as_deref(), Some(mac::FINDER));
        // Nothing at all: Finder.
        let mut ls = FakeLs(None, false);
        mac::run(Change::Make, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap();
        assert_eq!(mac::run(Change::Restore, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap(), "Folders open in Finder again");
        assert_eq!(ls.0.as_deref(), Some(mac::FINDER));
        // Changed since by something else: left alone.
        mac::run(Change::Make, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap();
        ls.0 = Some("com.other".into());
        let msg = mac::run(Change::Restore, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap();
        assert!(msg.contains("com.other is, set since"), "{msg}");
        assert_eq!(ls.0.as_deref(), Some("com.other"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Read-only: what LaunchServices says the folder viewer is (never changes it).
    #[cfg(target_os = "macos")]
    #[test]
    fn mac_reads_the_current_folder_viewer() {
        use mac::Handlers;
        eprintln!("public.folder viewer: {:?}, chosen: {:?}", mac::LaunchServices.current(), mac::LaunchServices.chosen());
        assert_eq!(mac::bundle_id(), None, "the test binary isn't an app bundle, so Make would explain instead");
    }
}
