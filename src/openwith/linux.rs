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
    let desktops: Vec<String> =
        std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().split(':').filter(|d| !d.is_empty()).map(|d| d.to_lowercase()).collect();
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
    desktop_paths(apps, id).iter().find_map(|p| read(p))
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
        let (Some(id), Some(d)) = (p.file_name().map(|n| n.to_string_lossy().into_owned()), read(&p).and_then(|t| parse_desktop(&t, lang)))
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
