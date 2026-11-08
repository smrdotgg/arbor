//! Moving files to the platform's trash. Returns where the item went when the platform says,
//! so a trash can be undone by moving it back.

use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
pub fn trash(path: &Path) -> Result<Option<PathBuf>, String> {
    use objc2_foundation::{NSFileManager, NSURL};
    let url = NSURL::from_file_path(path).ok_or("bad path")?;
    let fm = NSFileManager::defaultManager();
    let mut out = None;
    fm.trashItemAtURL_resultingItemURL_error(&url, Some(&mut out)).map_err(|e| e.localizedDescription().to_string())?;
    Ok(out.and_then(|u| u.to_file_path()))
}

/// The freedesktop.org trash spec: `$XDG_DATA_HOME/Trash/{files,info}` with a `.trashinfo`
/// per item. Items on another filesystem are refused rather than copied across.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn trash(path: &Path) -> Result<Option<PathBuf>, String> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| crate::fs::home().join(".local/share"));
    trash_in(&data.join("Trash"), path)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn trash_in(root: &Path, path: &Path) -> Result<Option<PathBuf>, String> {
    use std::io::Write;
    let (files, info) = (root.join("files"), root.join("info"));
    std::fs::create_dir_all(&files).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&info).map_err(|e| e.to_string())?;
    let name = path.file_name().ok_or("no file name")?.to_string_lossy().into_owned();
    // Reserve a unique name by creating its .trashinfo exclusively.
    let mut n = 1;
    let (stem, info_file) = loop {
        let stem = if n == 1 { name.clone() } else { format!("{name}.{n}") };
        let p = info.join(format!("{stem}.trashinfo"));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
            Ok(f) => break (stem, (p, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e.to_string()),
        }
    };
    let (info_path, mut f) = info_file;
    let abs = std::path::absolute(path).map_err(|e| e.to_string())?;
    let body = format!("[Trash Info]\nPath={}\nDeletionDate={}\n", url_escape(&abs), deletion_date());
    let dest = files.join(&stem);
    let res = f.write_all(body.as_bytes()).map_err(|e| e.to_string()).and_then(|_| std::fs::rename(path, &dest).map_err(|e| e.to_string()));
    if let Err(e) = res {
        let _ = std::fs::remove_file(&info_path);
        return Err(e);
    }
    Ok(Some(dest))
}

/// After an item at `trashed` (`Trash/files/NAME`) was put back: removes its
/// `Trash/info/NAME.trashinfo`, which would otherwise list it in the trash still.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn forget(trashed: &Path) {
    let (Some(files), Some(name)) = (trashed.parent(), trashed.file_name()) else { return };
    if files.file_name() != Some("files".as_ref()) {
        return;
    }
    let Some(root) = files.parent() else { return };
    let mut info = name.to_os_string();
    info.push(".trashinfo");
    let _ = std::fs::remove_file(root.join("info").join(info));
}

/// macOS keeps no record beside the item, and Windows trash can't be undone from Arbor.
#[cfg(not(all(unix, not(target_os = "macos"))))]
pub fn forget(_trashed: &Path) {}

#[cfg(all(unix, not(target_os = "macos")))]
fn url_escape(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut s = String::new();
    for &b in p.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

#[cfg(all(unix, not(target_os = "macos")))]
fn deletion_date() -> String {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    let t = rune::LocalTime::at(ms);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second)
}

/// The Recycle Bin through `SHFileOperationW` with undo allowed. Windows doesn't say where the
/// item went, so it can't be undone from Arbor (restore it from the Recycle Bin).
#[cfg(windows)]
pub fn trash(path: &Path) -> Result<Option<PathBuf>, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::{SHFILEOPSTRUCTW, SHFileOperationW};
    const FO_DELETE: u32 = 3;
    const FOF_SILENT: u16 = 0x4;
    const FOF_NOCONFIRMATION: u16 = 0x10;
    const FOF_ALLOWUNDO: u16 = 0x40;
    const FOF_NOERRORUI: u16 = 0x400;
    // Without it, FOF_NOCONFIRMATION silently deletes for good whatever can't be recycled (too
    // big, or a drive with no Recycle Bin). With it, Windows asks first.
    const FOF_WANTNUKEWARNING: u16 = 0x4000;
    let abs = std::path::absolute(path).map_err(|e| e.to_string())?;
    // Double-NUL-terminated list of one path.
    let mut from: Vec<u16> = abs.as_os_str().encode_wide().collect();
    from.extend([0, 0]);
    let mut op = SHFILEOPSTRUCTW {
        hwnd: std::ptr::null_mut(),
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        pTo: std::ptr::null(),
        fFlags: FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_WANTNUKEWARNING | FOF_SILENT | FOF_NOERRORUI,
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };
    // SAFETY: `op` points at a valid, double-NUL-terminated path buffer that outlives the call.
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 || op.fAnyOperationsAborted != 0 {
        return Err(format!("Recycle Bin refused (code {rc:#x})"));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn freedesktop_trash_and_restore_leave_no_record() {
        let d = std::env::temp_dir().join(format!("arbor-xdg-trash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("a b.txt");
        std::fs::write(&f, "x").unwrap();
        let at = super::trash_in(&d.join("Trash"), &f).unwrap().unwrap();
        assert_eq!(at, d.join("Trash/files/a b.txt"));
        let info = std::fs::read_to_string(d.join("Trash/info/a b.txt.trashinfo")).unwrap();
        assert!(info.contains("a%20b.txt"), "{info}");
        std::fs::rename(&at, &f).unwrap();
        super::forget(&at);
        assert!(!d.join("Trash/info/a b.txt.trashinfo").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    #[ignore = "moves a scratch file to the real Trash"]
    fn trashes_a_file() {
        let p = std::env::temp_dir().join(format!("arbor-trash-test-{}.txt", std::process::id()));
        std::fs::write(&p, "x").unwrap();
        let t = std::time::Instant::now();
        let r = super::trash(&p);
        eprintln!("trash took {:?}: {r:?}", t.elapsed());
        assert!(r.is_ok());
        assert!(!p.exists());
    }
}
