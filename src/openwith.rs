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
            let child = std::process::Command::new(&argv[0]).args(&argv[1..]).spawn().map_err(|e| format!("{}: {e}", argv[0]))?;
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
pub mod xdg {
    #![cfg_attr(not(any(test, all(unix, not(target_os = "macos")))), allow(dead_code))]

    use super::Handler;
    use std::path::PathBuf;

    /// The MIME type `globs2` gives `name` (its `weight:type:glob` lines): the highest weight wins,
    /// then the longest pattern. Patterns match the whole name, case aside unless the line ends in
    /// `:cs`.
    pub fn mime_from_globs(globs2: &str, name: &str) -> Option<(u32, String)> {
        let lower = name.to_lowercase();
        // A case-sensitive match beats one that ignores case (the spec tries them in that order).
        let mut best: Option<(u32, bool, usize, String)> = None;
        for line in globs2.lines() {
            if line.starts_with('#') {
                continue;
            }
            let mut parts = line.splitn(4, ':');
            let (Some(w), Some(mime), Some(glob)) = (parts.next(), parts.next(), parts.next()) else { continue };
            let Ok(w) = w.parse::<u32>() else { continue };
            let cs = parts.next().is_some_and(|f| f.split(',').any(|f| f == "cs"));
            let hit = if cs { glob_match(glob, name) } else { glob_match(&glob.to_lowercase(), &lower) };
            if !hit {
                continue;
            }
            let better = best.as_ref().is_none_or(|(bw, bcs, bl, _)| (w, cs, glob.len()) > (*bw, *bcs, *bl));
            if better {
                best = Some((w, cs, glob.len(), mime.to_string()));
            }
        }
        best.map(|(w, _, _, m)| (w, m))
    }

    /// A shell glob over a whole name: `*`, `?` and `[…]`.
    fn glob_match(glob: &str, name: &str) -> bool {
        crate::pattern::Glob::name(glob, crate::pattern::Case::Sensitive).is_ok_and(|g| g.matches(name))
    }

    /// One `mimeapps.list`: its three sections, each `type=a.desktop;b.desktop;`.
    #[derive(Debug, Default, PartialEq)]
    pub struct MimeApps {
        pub defaults: Vec<(String, Vec<String>)>,
        pub added: Vec<(String, Vec<String>)>,
        pub removed: Vec<(String, Vec<String>)>,
    }

    /// Reads `[section]` → `type=id;id;` lists from a key file (mimeapps.list, mimeinfo.cache).
    fn key_lists(text: &str, mut section: impl FnMut(&str) -> Option<usize>) -> Vec<Vec<(String, Vec<String>)>> {
        let mut out: Vec<Vec<(String, Vec<String>)>> = vec![Vec::new(), Vec::new(), Vec::new()];
        let mut at: Option<usize> = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                at = section(name);
                continue;
            }
            let (Some(i), Some((k, v))) = (at, line.split_once('=')) else { continue };
            let ids: Vec<String> = v.split(';').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
            out[i].push((k.trim().to_string(), ids));
        }
        out
    }

    pub fn parse_mimeapps(text: &str) -> MimeApps {
        let mut l = key_lists(text, |s| match s {
            "Default Applications" => Some(0),
            "Added Associations" => Some(1),
            "Removed Associations" => Some(2),
            _ => None,
        })
        .into_iter();
        MimeApps { defaults: l.next().unwrap_or_default(), added: l.next().unwrap_or_default(), removed: l.next().unwrap_or_default() }
    }

    /// `mimeinfo.cache`'s `[MIME Cache]`: each type's apps.
    pub fn parse_mimeinfo_cache(text: &str) -> Vec<(String, Vec<String>)> {
        key_lists(text, |s| (s == "MIME Cache").then_some(0)).swap_remove(0)
    }

    /// What Open With needs from a `.desktop` file.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Desktop {
        pub name: String,
        pub exec: String,
        pub mimes: Vec<String>,
        /// `Hidden=true`: deleted, as if it weren't there.
        pub hidden: bool,
    }

    /// The `[Desktop Entry]` of a desktop file, for an application with an `Exec` line. `lang` is
    /// `$LANG` (`de_DE.UTF-8`), for a translated name.
    pub fn parse_desktop(text: &str, lang: &str) -> Option<Desktop> {
        let lang = lang.split(['.', '@']).next().unwrap_or("");
        let short = lang.split('_').next().unwrap_or("");
        let (mut name, mut name_full, mut name_short) = (None, None, None);
        let (mut exec, mut mimes, mut hidden, mut app) = (None, Vec::new(), false, true);
        let mut inside = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                inside = line == "[Desktop Entry]";
                continue;
            }
            let Some((k, v)) = line.split_once('=').filter(|_| inside) else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "Name" => name = Some(v.to_string()),
                "Exec" => exec = Some(unescape_value(v)),
                "MimeType" => mimes = v.split(';').filter(|s| !s.is_empty()).map(String::from).collect(),
                "Hidden" => hidden = v == "true",
                "Type" => app = v == "Application",
                _ if !lang.is_empty() && k == format!("Name[{lang}]") => name_full = Some(v.to_string()),
                _ if !short.is_empty() && k == format!("Name[{short}]") => name_short = Some(v.to_string()),
                _ => {}
            }
        }
        let name = name_full.or(name_short).or(name)?;
        Some(Desktop { name, exec: exec.filter(|e| !e.is_empty() && app)?, mimes, hidden })
    }

    /// A desktop file value's escapes: `\s` `\n` `\t` `\r` `\\`.
    pub fn unescape_value(v: &str) -> String {
        let mut out = String::with_capacity(v.len());
        let mut it = v.chars();
        while let Some(c) = it.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match it.next() {
                Some('s') => out.push(' '),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some(o) => {
                    out.push('\\');
                    out.push(o);
                }
                None => out.push('\\'),
            }
        }
        out
    }

    /// The command lines an `Exec` line gives for `files`: its quoting undone, `%f`/`%u` with one
    /// file per run (so one run for each file), `%F`/`%U` with all of them, `%%` a percent sign, and
    /// the other field codes dropped. A line with no file field gets the files at the end.
    pub fn exec_argv(exec: &str, files: &[PathBuf]) -> Vec<Vec<String>> {
        // Split into arguments: "…" quotes, where \" \` \$ \\ are escapes.
        let mut args: Vec<String> = Vec::new();
        let (mut cur, mut quoted, mut any) = (String::new(), false, false);
        let mut it = exec.chars().peekable();
        while let Some(c) = it.next() {
            match c {
                '"' => {
                    quoted = !quoted;
                    any = true;
                }
                '\\' if quoted && it.peek().is_some_and(|n| matches!(n, '"' | '`' | '$' | '\\')) => cur.push(it.next().unwrap_or('\\')),
                c if c.is_whitespace() && !quoted => {
                    if any || !cur.is_empty() {
                        args.push(std::mem::take(&mut cur));
                    }
                    any = false;
                }
                c => cur.push(c),
            }
        }
        if any || !cur.is_empty() {
            args.push(cur);
        }
        let paths: Vec<String> = files.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        let single = args.iter().any(|a| a.contains("%f") || a.contains("%u"));
        let multi = args.iter().any(|a| a == "%F" || a == "%U");
        let expand = |one: Option<&str>| -> Vec<String> {
            let mut out = Vec::new();
            for a in &args {
                if a == "%F" || a == "%U" {
                    out.extend(paths.iter().cloned());
                    continue;
                }
                let mut s = String::new();
                let mut ch = a.chars();
                let mut dropped = false;
                while let Some(c) = ch.next() {
                    if c != '%' {
                        s.push(c);
                        continue;
                    }
                    match ch.next() {
                        Some('%') => s.push('%'),
                        Some('f' | 'u') => s.push_str(one.unwrap_or("")),
                        Some(_) | None => dropped = true,
                    }
                }
                if !(s.is_empty() && dropped) {
                    out.push(s);
                }
            }
            if !single && !multi {
                out.extend(paths.iter().cloned());
            }
            out
        };
        let runs: Vec<Vec<String>> = if single && !multi { paths.iter().map(|p| expand(Some(p))).collect() } else { vec![expand(None)] };
        runs.into_iter().filter(|r| !r.is_empty()).collect()
    }

    /// The apps for `mime`, best first: the first default (in the files' order) that exists and
    /// isn't removed, then the added associations, then the caches'. `mimeapps` is in the spec's
    /// order (the user's first); `find` reads a desktop file by id.
    pub fn resolve(
        mime: &str,
        mimeapps: &[MimeApps],
        caches: &[Vec<(String, Vec<String>)>],
        find: impl Fn(&str) -> Option<Desktop>,
    ) -> Vec<Handler> {
        let removed: Vec<&String> =
            mimeapps.iter().flat_map(|m| m.removed.iter()).filter(|(t, _)| t == mime).flat_map(|(_, ids)| ids).collect();
        let mut out: Vec<Handler> = Vec::new();
        let add = |id: &str, default: bool, out: &mut Vec<Handler>| {
            if removed.iter().any(|r| *r == id) || out.iter().any(|h| h.id == id) {
                return false;
            }
            let Some(d) = find(id).filter(|d| !d.hidden) else { return false };
            out.push(Handler { name: d.name, id: id.to_string(), default, exec: Some(d.exec) });
            true
        };
        'defaults: for m in mimeapps {
            for (t, ids) in &m.defaults {
                if t != mime {
                    continue;
                }
                for id in ids {
                    if add(id, true, &mut out) {
                        break 'defaults;
                    }
                }
            }
        }
        let assoc = mimeapps.iter().flat_map(|m| m.added.iter()).chain(caches.iter().flatten());
        for (t, ids) in assoc {
            if t == mime {
                for id in ids {
                    add(id, false, &mut out);
                }
            }
        }
        out
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod linux {
    use super::xdg::*;
    use super::*;
    use std::ffi::OsStr;

    fn env_dir(var: &str, default: impl FnOnce() -> PathBuf) -> PathBuf {
        std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(default)
    }

    fn env_dirs(var: &str, default: &str) -> Vec<PathBuf> {
        let v = std::env::var_os(var).filter(|v| !v.is_empty()).unwrap_or_else(|| default.into());
        std::env::split_paths(&v).filter(|p| p.is_absolute()).collect()
    }

    fn read(p: &Path) -> Option<String> {
        std::fs::read_to_string(p).ok()
    }

    /// The file's MIME type: `xdg-mime` if it's installed, else the MIME database's globs, else
    /// text or not by its first bytes.
    pub fn mime_of(file: &Path, data: &[PathBuf]) -> String {
        if let Some(x) = crate::volumes::find_program("xdg-mime", std::env::var_os("PATH").as_deref())
            && let Ok(out) = std::process::Command::new(x).args(["query", "filetype"]).arg(file).output()
            && out.status.success()
        {
            let t = String::from_utf8_lossy(&out.stdout);
            let t = t.split(';').next().unwrap_or("").trim();
            if t.contains('/') {
                return t.to_string();
            }
        }
        let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let best = data
            .iter()
            .filter_map(|d| read(&d.join("mime/globs2")))
            .filter_map(|g| mime_from_globs(&g, &name))
            .fold(None::<(u32, String)>, |b, x| if b.as_ref().is_none_or(|b| x.0 > b.0) { Some(x) } else { b });
        if let Some((_, m)) = best {
            return m;
        }
        let mut head = vec![0u8; 8192];
        let n = std::fs::File::open(file).and_then(|mut f| std::io::Read::read(&mut f, &mut head)).unwrap_or(0);
        if n == 0 || crate::preview::sniff_text(&head[..n], n == head.len()).is_some() {
            "text/plain".into()
        } else {
            "application/octet-stream".into()
        }
    }

    pub fn list(file: &Path) -> Result<Vec<Handler>, String> {
        let home = crate::fs::home();
        let config_home = env_dir("XDG_CONFIG_HOME", || home.join(".config"));
        let data_home = env_dir("XDG_DATA_HOME", || home.join(".local/share"));
        let config_dirs = env_dirs("XDG_CONFIG_DIRS", "/etc/xdg");
        let data_dirs = env_dirs("XDG_DATA_DIRS", "/usr/local/share:/usr/share");
        let mut data = vec![data_home.clone()];
        data.extend(data_dirs.iter().cloned());
        let mime = mime_of(file, &data);
        let desktops: Vec<String> = std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .split(':')
            .filter(|d| !d.is_empty())
            .map(|d| d.to_lowercase())
            .collect();
        // mimeapps.list, the user's first (the spec's order).
        let mut lists: Vec<PathBuf> = Vec::new();
        for dir in std::iter::once(&config_home).chain(config_dirs.iter()) {
            lists.extend(desktops.iter().map(|d| dir.join(format!("{d}-mimeapps.list"))));
            lists.push(dir.join("mimeapps.list"));
        }
        let apps: Vec<PathBuf> = data.iter().map(|d| d.join("applications")).collect();
        for dir in &apps {
            lists.extend(desktops.iter().map(|d| dir.join(format!("{d}-mimeapps.list"))));
            lists.push(dir.join("mimeapps.list"));
        }
        let mimeapps: Vec<MimeApps> = lists.iter().filter_map(|p| read(p)).map(|t| parse_mimeapps(&t)).collect();
        let lang = std::env::var("LC_ALL").or_else(|_| std::env::var("LC_MESSAGES")).or_else(|_| std::env::var("LANG")).unwrap_or_default();
        let find = |id: &str| find_desktop(&apps, id).and_then(|t| parse_desktop(&t, &lang));
        let caches: Vec<Vec<(String, Vec<String>)>> = apps.iter().map(|d| cache_or_scan(d, &lang)).collect();
        let mut out = resolve(&mime, &mimeapps, &caches, find);
        // Text of any kind opens in a text editor too.
        if mime.starts_with("text/") && mime != "text/plain" {
            for h in resolve("text/plain", &mimeapps, &caches, find) {
                if !out.iter().any(|o| o.id == h.id) {
                    out.push(Handler { default: false, ..h });
                }
            }
        }
        if out.is_empty() {
            return Err(format!("No apps are set up to open {mime} files"));
        }
        Ok(out)
    }

    /// A desktop file by id, in the first applications folder that has it (`a-b.desktop` may
    /// be `a/b.desktop`).
    fn find_desktop(apps: &[PathBuf], id: &str) -> Option<String> {
        for dir in apps {
            if let Some(t) = read(&dir.join(id)) {
                return Some(t);
            }
            if let Some((sub, rest)) = id.split_once('-')
                && let Some(t) = read(&dir.join(sub).join(rest))
            {
                return Some(t);
            }
        }
        None
    }

    /// The folder's `mimeinfo.cache`, or its desktop files' `MimeType` lines when it has none.
    fn cache_or_scan(dir: &Path, lang: &str) -> Vec<(String, Vec<String>)> {
        if let Some(t) = read(&dir.join("mimeinfo.cache")) {
            return parse_mimeinfo_cache(&t);
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
        let mut out: Vec<(String, Vec<String>)> = Vec::new();
        for de in rd.flatten() {
            let p = de.path();
            if p.extension() != Some(OsStr::new("desktop")) {
                continue;
            }
            let (Some(id), Some(d)) =
                (p.file_name().map(|n| n.to_string_lossy().into_owned()), read(&p).and_then(|t| parse_desktop(&t, lang)))
            else {
                continue;
            };
            for m in d.mimes {
                match out.iter_mut().find(|(t, _)| *t == m) {
                    Some((_, ids)) => ids.push(id.clone()),
                    None => out.push((m, vec![id.clone()])),
                }
            }
        }
        out
    }
}

// ---- macOS --------------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod mac {
    use super::Handler;
    use objc2::rc::autoreleasepool;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send, sel};
    use objc2_foundation::NSString;
    use std::path::Path;

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn LSCopyApplicationURLsForURL(url: *const AnyObject, roles: u32) -> *mut AnyObject;
        fn CFRelease(cf: *const std::ffi::c_void);
    }

    fn path_of(url: &AnyObject) -> Option<String> {
        // SAFETY: an NSURL; `path` is nil-safe and autoreleased.
        unsafe {
            let p: *mut NSString = msg_send![url, path];
            p.as_ref().map(|p| p.to_string())
        }
    }

    pub fn list(file: &Path) -> Result<Vec<Handler>, String> {
        autoreleasepool(|_| {
            // SAFETY: NSWorkspace may be used from any thread; every object is checked for nil,
            // the array Launch Services copies is released, and the rest are autoreleased.
            unsafe {
                let ws: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
                let ws = ws.as_ref().ok_or("NSWorkspace isn't available")?;
                let p = NSString::from_str(&file.to_string_lossy());
                let url: *mut AnyObject = msg_send![class!(NSURL), fileURLWithPath: &*p];
                let url = url.as_ref().ok_or("not a file URL")?;
                let default: *mut AnyObject = msg_send![ws, URLForApplicationToOpenURL: url];
                let default = default.as_ref().and_then(path_of);
                let modern: bool = msg_send![ws, respondsToSelector: sel!(URLsForApplicationsToOpenURL:)];
                let (arr, owned): (*mut AnyObject, bool) = if modern {
                    (msg_send![ws, URLsForApplicationsToOpenURL: url], false)
                } else {
                    (LSCopyApplicationURLsForURL(url, 0xFFFF_FFFF), true)
                };
                let mut paths: Vec<String> = Vec::new();
                if let Some(a) = arr.as_ref() {
                    let n: usize = msg_send![a, count];
                    for i in 0..n {
                        let u: *mut AnyObject = msg_send![a, objectAtIndex: i];
                        if let Some(p) = u.as_ref().and_then(path_of)
                            && !paths.contains(&p)
                        {
                            paths.push(p);
                        }
                    }
                    if owned {
                        CFRelease((a as *const AnyObject).cast());
                    }
                }
                if let Some(d) = &default {
                    paths.retain(|p| p != d);
                    paths.insert(0, d.clone());
                }
                let fm: *mut AnyObject = msg_send![class!(NSFileManager), defaultManager];
                let out: Vec<Handler> = paths
                    .into_iter()
                    .map(|p| {
                        let ns = NSString::from_str(&p);
                        let shown: *mut NSString = match fm.as_ref() {
                            Some(fm) => msg_send![fm, displayNameAtPath: &*ns],
                            None => std::ptr::null_mut(),
                        };
                        let name = shown.as_ref().map(|s| s.to_string()).unwrap_or_else(|| p.clone());
                        let name = name.strip_suffix(".app").unwrap_or(&name).to_string();
                        Handler { default: Some(&p) == default.as_ref(), name, id: p, exec: None }
                    })
                    .collect();
                if out.is_empty() { Err("No apps can open this file".into()) } else { Ok(out) }
            }
        })
    }
}

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
    #[cfg_attr(windows, allow(dead_code))]
    fn read_apps(&mut self, req: u64, file: PathBuf) -> Option<(u64, Result<Vec<Handler>, String>)> {
        crate::folder::read_waiting(&self.apps_loader, self.show_wait, move || Some((req, list(&file))))
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
mod tests {
    use super::xdg::*;
    use super::*;

    const GLOBS: &str = "\
# This file was automatically generated
50:text/plain:*.txt
50:text/x-csrc:*.c
50:text/x-c++src:*.C:cs
50:image/png:*.png
60:application/x-compressed-tar:*.tar.gz
50:application/gzip:*.gz
55:text/x-makefile:makefile
10:text/x-readme:README*
";

    #[test]
    fn globs2_picks_the_weightiest_then_longest_match() {
        assert_eq!(mime_from_globs(GLOBS, "notes.TXT").map(|m| m.1).as_deref(), Some("text/plain"));
        assert_eq!(mime_from_globs(GLOBS, "a.tar.gz").map(|m| m.1).as_deref(), Some("application/x-compressed-tar"));
        assert_eq!(mime_from_globs(GLOBS, "a.gz").map(|m| m.1).as_deref(), Some("application/gzip"));
        assert_eq!(mime_from_globs(GLOBS, "x.C").map(|m| m.1).as_deref(), Some("text/x-c++src"), "case-sensitive line");
        assert_eq!(mime_from_globs(GLOBS, "x.c").map(|m| m.1).as_deref(), Some("text/x-csrc"));
        assert_eq!(mime_from_globs(GLOBS, "Makefile").map(|m| m.1).as_deref(), Some("text/x-makefile"));
        assert_eq!(mime_from_globs(GLOBS, "README.md").map(|m| m.1).as_deref(), Some("text/x-readme"));
        assert_eq!(mime_from_globs(GLOBS, "photo.jpeg"), None);
    }

    const USER_LIST: &str = "\
[Default Applications]
text/plain=gone.desktop;org.gnome.TextEditor.desktop;
image/png=org.gnome.Loupe.desktop

[Added Associations]
text/plain=vim.desktop;code.desktop;

[Removed Associations]
text/plain=nano.desktop;
";
    const SYSTEM_LIST: &str = "[Default Applications]\ntext/plain=gedit.desktop\n";
    const CACHE: &str =
        "[MIME Cache]\ntext/plain=gedit.desktop;nano.desktop;hidden.desktop;org.gnome.TextEditor.desktop;\nimage/png=gimp.desktop;\n";

    fn desktop(id: &str) -> Option<Desktop> {
        let text = match id {
            "org.gnome.TextEditor.desktop" => {
                "[Desktop Entry]\nType=Application\nName=Text Editor\nName[de]=Texteditor\nExec=gnome-text-editor %U\nMimeType=text/plain;\n"
            }
            "vim.desktop" => "[Desktop Entry]\nName=Vim\nExec=vim %F\nTerminal=true\n",
            "code.desktop" => {
                "[Desktop Entry]\nName=Visual Studio Code\nExec=/usr/share/code/code --unity-launch %F\n[Desktop Action new]\nName=New Window\nExec=code --new-window\n"
            }
            "gedit.desktop" => "[Desktop Entry]\nName=gedit\nExec=gedit %U\n",
            "nano.desktop" => "[Desktop Entry]\nName=GNU nano\nExec=nano %F\n",
            "hidden.desktop" => "[Desktop Entry]\nName=Hidden\nExec=hidden %f\nHidden=true\n",
            "gimp.desktop" => "[Desktop Entry]\nName=GIMP\nExec=gimp-2.10 %U\n",
            _ => return None,
        };
        parse_desktop(text, "en_US.UTF-8")
    }

    /// The default from the user's list (skipping one whose file is gone), then the added
    /// associations, then the cache, without removed or hidden apps or the same app twice.
    #[test]
    fn linux_handlers_follow_the_spec_order() {
        let lists = [parse_mimeapps(USER_LIST), parse_mimeapps(SYSTEM_LIST)];
        let caches = [parse_mimeinfo_cache(CACHE)];
        let got = resolve("text/plain", &lists, &caches, desktop);
        let names: Vec<(&str, bool)> = got.iter().map(|h| (h.name.as_str(), h.default)).collect();
        assert_eq!(names, [("Text Editor", true), ("Vim", false), ("Visual Studio Code", false), ("gedit", false)]);
        assert_eq!(got[2].exec.as_deref(), Some("/usr/share/code/code --unity-launch %F"), "the main entry's Exec, not an action's");
        let png = resolve("image/png", &lists, &caches, desktop);
        assert_eq!(png.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["GIMP"], "a default whose file is missing is skipped");
        assert!(resolve("video/mp4", &lists, &caches, desktop).is_empty());
    }

    #[test]
    fn desktop_files_give_a_translated_name() {
        let text = "[Desktop Entry]\nName=Text Editor\nName[de]=Texteditor\nName[de_AT]=Texteditor (AT)\nExec=te %U\n";
        assert_eq!(parse_desktop(text, "de_DE.UTF-8").unwrap().name, "Texteditor");
        assert_eq!(parse_desktop(text, "de_AT.UTF-8").unwrap().name, "Texteditor (AT)");
        assert_eq!(parse_desktop(text, "C").unwrap().name, "Text Editor");
        assert!(parse_desktop("[Desktop Entry]\nName=Link\nType=Link\nExec=x\n", "C").is_none());
        assert!(parse_desktop("[Desktop Entry]\nName=No exec\n", "C").is_none());
    }

    #[test]
    fn exec_lines_expand_their_field_codes() {
        let f = |v: &[&str]| v.iter().map(PathBuf::from).collect::<Vec<_>>();
        let s = |v: Vec<Vec<String>>| v.into_iter().map(|r| r.join("|")).collect::<Vec<_>>();
        assert_eq!(s(exec_argv("gedit %U", &f(&["/a b.txt", "/c"]))), ["gedit|/a b.txt|/c"]);
        assert_eq!(s(exec_argv("viewer --one %f", &f(&["/a", "/b"]))), ["viewer|--one|/a", "viewer|--one|/b"]);
        assert_eq!(s(exec_argv("\"/opt/My App/run\" --file=%f %i %c", &f(&["/a"]))), ["/opt/My App/run|--file=/a"]);
        assert_eq!(s(exec_argv("sh -c \"echo \\\"hi\\\" 100%%\" %F", &f(&["/a"]))), ["sh|-c|echo \"hi\" 100%|/a"]);
        assert_eq!(s(exec_argv("plain", &f(&["/a"]))), ["plain|/a"], "no field code: files at the end");
        assert_eq!(unescape_value("a\\sb\\\\c"), "a b\\c");
    }
}
