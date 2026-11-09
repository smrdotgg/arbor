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
    is_ours_bytes(text.as_bytes())
}

fn is_ours_bytes(b: &[u8]) -> bool {
    b.starts_with(MARKER.as_bytes())
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
                    .call_method(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", Some("org.freedesktop.DBus"), "ReloadConfig", &())
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

/// A key file Arbor edits (`mimeapps.list`): its text, `None` when there's none. One that
/// isn't UTF-8 (the spec says it is) is an error: rewriting it would turn every byte that isn't
/// into U+FFFD, changing lines of the user's that aren't Arbor's.
fn read(path: &Path) -> Result<Option<String>, String> {
    match read_bytes(path)? {
        Some(b) => {
            String::from_utf8(b).map(Some).map_err(|_| format!("{} isn't text (UTF-8), so nothing was changed", crate::fs::pretty(path)))
        }
        None => Ok(None),
    }
}

/// A file's bytes as they are (the activation file another file manager wrote, kept and put
/// back exactly), `None` when there's none.
fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", crate::fs::pretty(path))),
    }
}

/// The folder default a backup recorded, as a value for `mimeapps.list`: one line of text. A
/// line break in it would add lines of its own to the file, so it's damage.
pub(crate) fn saved_default(b: &Backup, backup: &Path) -> Result<Option<String>, String> {
    match b.get(MIME_KEY).flatten() {
        None => Ok(None),
        Some(v) => match std::str::from_utf8(v) {
            Ok(t) if !t.chars().any(char::is_control) => Ok(Some(t.to_string())),
            _ => Err(format!(
                "{} is damaged (the folder default it recorded isn't one line of text), so nothing was changed",
                crate::fs::pretty(backup)
            )),
        },
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
    let old_text = read(&mimeapps)?;
    let had_mimeapps = old_text.is_some();
    let text = old_text.unwrap_or_default();
    let service = service_file(&d.data_home);
    let old_service = read_bytes(&service)?;
    // The backup is on the disk before anything changes.
    let fresh = Backup::require(Backup::read(backup)?, &required(), backup)?.is_none();
    if fresh {
        let mut b = Backup::default();
        b.put(MIME_KEY, default_app(&text, MIME).as_deref().map(str::as_bytes));
        b.put(SERVICE_KEY, old_service.as_deref().filter(|t| !is_ours_bytes(t)));
        b.write(backup)?;
    }
    let new_text = set_default_app(&text, MIME, Some(DESKTOP_ID));
    let written = write_file(&mimeapps, new_text.as_bytes()).and_then(|()| write_file(&service, service_text(&d.exe).as_bytes()));
    if let Err(e) = written {
        // Halfway: the folder default goes back to what it was (if it's still what Arbor
        // wrote), and a backup made just now goes too.
        let back = match read(&mimeapps) {
            Ok(Some(now)) if now == new_text && now != text => {
                if had_mimeapps {
                    write_file(&mimeapps, text.as_bytes())
                } else {
                    std::fs::remove_file(&mimeapps).map_err(|e| e.to_string())
                }
            }
            _ => Ok(()),
        };
        return Err(match back {
            Ok(()) => {
                if fresh {
                    let _ = remove_backup(backup);
                }
                format!("Couldn't make Arbor the default for folders ({e}), so nothing was changed")
            }
            Err(e2) => format!(
                "Couldn't make Arbor the default for folders ({e}), and putting the folder default back failed too ({e2}): run Restore the System Default for Folders once that's fixed"
            ),
        });
    }
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

/// What every backup Make writes here records.
fn required() -> Vec<String> {
    vec![MIME_KEY.into(), SERVICE_KEY.into()]
}

fn restore(d: &Dirs, backup: &Path) -> Result<String, String> {
    let saved = Backup::require(Backup::read(backup)?, &required(), backup)?;
    let b = saved.clone().unwrap_or_default();
    let (mut kept, mut changed) = (Vec::new(), false);
    let mimeapps = d.config_home.join("mimeapps.list");
    let before = saved_default(&b, backup)?;
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
    if let Some(text) = read_bytes(&service)? {
        if is_ours_bytes(&text) {
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
