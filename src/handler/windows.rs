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
///
/// In the order it's written: the commands first, then what makes them the default (the
/// `shell` keys' default verb, and Win+E's `DelegateExecute`), so a write that fails midway
/// never leaves a default pointing at a command that isn't there.
pub fn plan(root: &str, exe: &str) -> Vec<Set> {
    let set = |key: String, name: &'static str, data: String| Set { key, name, data };
    let mut v = Vec::new();
    let classes = ["Directory", "Drive"];
    for class in classes {
        let shell = format!(r"{root}\{class}\shell");
        v.push(set(format!(r"{shell}\{VERB}"), "", "Open in Arbor".into()));
        v.push(set(format!(r"{shell}\{VERB}"), "Icon", format!("{exe},0")));
        v.push(set(format!(r"{shell}\{VERB}\command"), "", format!("\"{exe}\" \"%1\"")));
    }
    let win_e = format!(r"{root}\CLSID\{WIN_E}\shell\opennewwindow\command");
    v.push(set(win_e.clone(), "", format!("\"{exe}\"")));
    for class in classes {
        v.push(set(format!(r"{root}\{class}\shell"), "", VERB.into()));
    }
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

/// The backup for writing `plan`: what it replaces (unless `backup` already has it: Arbor
/// was the default already, and the first backup stays) and what it writes. Changes nothing;
/// saved before [`apply`] runs.
pub fn snapshot(store: &dyn Store, plan: &[Set], root: &str, backup: Option<Backup>) -> Backup {
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
        b.put(&wrote_key(s), Some(s.data.as_bytes()));
    }
    b
}

/// Writes `plan`, in order.
pub fn apply(store: &mut dyn Store, plan: &[Set]) -> Result<(), String> {
    plan.iter().try_for_each(|s| store.set(&s.key, s.name, REG_SZ, &utf16z(&s.data)))
}

/// [`snapshot`] then [`apply`] (tests; `run` saves the snapshot in between).
#[cfg(test)]
pub fn make(store: &mut dyn Store, plan: &[Set], root: &str, backup: Option<Backup>) -> Result<Backup, String> {
    let b = snapshot(store, plan, root, backup);
    apply(store, plan)?;
    Ok(b)
}

/// What every backup Make writes records for `plan`: whether each key existed, and each
/// value before.
fn required(plan: &[Set], root: &str) -> Vec<String> {
    keys(plan, root).iter().map(|k| existed_key(k)).chain(plan.iter().map(before_key)).collect()
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
    let saved = Backup::require(Backup::read(backup)?, &required(&plan, root), backup)?;
    match change {
        Change::Make => {
            // The whole snapshot is on the disk before the registry changes at all.
            let first = saved.clone();
            // What this attempt replaces, to put back if a write fails: the registry just
            // before it. When Arbor is the default already (Make again, from another
            // arbor.exe), that's the first Make's values, not the durable backup's.
            let attempt = snapshot(store, &plan, root, None);
            let b = snapshot(store, &plan, root, saved);
            b.write(backup)?;
            if let Err(e) = apply(store, &plan) {
                // Halfway: what this attempt wrote goes back, and so does the backup file
                // (none, or the first Make's, as it was).
                return Err(match restore(store, &plan, root, Some(&attempt)) {
                    Ok(_) => {
                        match &first {
                            None => {
                                let _ = remove_backup(backup);
                            }
                            Some(f) => {
                                let _ = f.write(backup);
                            }
                        }
                        format!("Couldn't make Arbor the default for folders ({e}), so nothing was changed")
                    }
                    Err(e2) => format!(
                        "Couldn't make Arbor the default for folders ({e}), and putting back what was written failed too ({e2}). The backup is kept: run Restore the System Default for Folders once that's fixed"
                    ),
                });
            }
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
use crate::ffi::wide;

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
        unsafe {
            let mut r = RegQueryValueExW(h, n.as_ptr(), std::ptr::null(), &mut kind, std::ptr::null_mut(), &mut len);
            let mut data = vec![0u8; len as usize];
            if r == 0 && len > 0 {
                r = RegQueryValueExW(h, n.as_ptr(), std::ptr::null(), &mut kind, data.as_mut_ptr(), &mut len);
                data.truncate(len as usize);
            }
            RegCloseKey(h);
            (r == 0).then_some((kind, data))
        }
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
        use std::ptr::{null, null_mut};
        // SAFETY: the open key; only the counts are asked for, the rest are null.
        let r = unsafe {
            let r = RegQueryInfoKeyW(
                h,
                null_mut(),
                null_mut(),
                null(),
                &mut subkeys,
                null_mut(),
                null_mut(),
                &mut values,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
            );
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
